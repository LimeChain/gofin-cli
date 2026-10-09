use fin_binance::{
    BinanceEndpoint, BinanceError, BinanceRequest, ExactDecimal, FundingCompleteTime,
    FundingHistoryRow, HistoryWindow, StandardWithdrawalIntent, TransferHistoryResponse,
    UniversalTransferIntent, VenueIdentifier, parse_response,
};
use serde_json::{Value, json};

#[path = "funding_contract/generic_plans.rs"]
mod generic_plan_tests;
#[path = "funding_contract/withdrawal_rejection.rs"]
mod withdrawal_rejection_tests;

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
fn withdrawal_request_freezes_selected_network_spot_and_exact_quantity() {
    let request = StandardWithdrawalIntent {
        coin: "ETH".into(),
        network: "ETH".into(),
        amount: amount("0.005000000000000001"),
        address: "0x1111111111111111111111111111111111111111".into(),
        memo: None,
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
fn withdrawal_request_supports_native_bitcoin_arbitrum_and_tagged_xrp_without_substitution() {
    for (coin, network, address, memo) in [
        ("BTC", "BTC", "bc1qsyntheticfixtureaddress", None),
        (
            "ETH",
            "ARBITRUM",
            "0x1111111111111111111111111111111111111111",
            None,
        ),
        ("XRP", "XRP", "rSyntheticFixtureAddress", Some("00123456")),
        ("币", "网络", "synthetic-address", Some("memo 雪+&%=tag")),
    ] {
        let client_id = fin_binance::ClientIdentity::withdrawal("generic-request-000001").unwrap();
        let request = StandardWithdrawalIntent {
            coin: coin.into(),
            network: network.into(),
            amount: amount("0.123456789012345678901234567890"),
            address: address.into(),
            memo: memo.map(str::to_owned),
            client_id: client_id.clone(),
        }
        .request()
        .unwrap();
        assert_eq!(
            request.endpoint.method_path(),
            ("POST", "/sapi/v1/capital/withdraw/apply")
        );
        assert_eq!(request.get("coin"), Some(coin));
        assert_eq!(request.get("network"), Some(network));
        assert_eq!(request.get("address"), Some(address));
        assert_eq!(request.get("addressTag"), memo);
        assert_eq!(
            request.get("amount"),
            Some("0.12345678901234567890123456789")
        );
        assert_eq!(request.get("walletType"), Some("0"));
        assert_eq!(request.get("withdrawOrderId"), Some(client_id.as_str()));
        let recovered: BinanceRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(
            recovered, request,
            "persisted request retains the exact route, memo and identity"
        );
    }
}

#[test]
fn withdrawal_request_rejects_malformed_fields_duplicate_parameters_and_arbitrary_routing() {
    let request = StandardWithdrawalIntent {
        coin: "XRP".into(),
        network: "XRP".into(),
        amount: amount("1"),
        address: "rSyntheticFixtureAddress".into(),
        memo: Some("123456".into()),
        client_id: fin_binance::ClientIdentity::withdrawal("validation-request-000001").unwrap(),
    }
    .request()
    .unwrap();
    let replace = |key: &str, value: &str| {
        let mut changed = request.clone();
        changed
            .params
            .iter_mut()
            .find(|param| param.key == key)
            .unwrap()
            .value = value.into();
        changed
    };
    for (key, max_bytes) in [
        ("coin", 40),
        ("network", 40),
        ("address", 256),
        ("addressTag", 256),
    ] {
        for value in [
            "",
            " leading",
            "trailing ",
            "\u{2003}leading",
            "line\nfeed",
            "tab\tvalue",
            "null\0byte",
        ] {
            assert_eq!(
                replace(key, value).validate(),
                Err(BinanceError::InvalidRequest),
                "{key}={value:?}"
            );
        }
        assert!(replace(key, &"a".repeat(max_bytes)).validate().is_ok());
        assert_eq!(
            replace(key, &"a".repeat(max_bytes + 1)).validate(),
            Err(BinanceError::InvalidRequest)
        );
        assert_eq!(
            replace(key, &"雪".repeat(max_bytes / 3 + 1)).validate(),
            Err(BinanceError::InvalidRequest),
            "bound uses UTF-8 bytes"
        );
        assert_eq!(
            request.clone().push(key, "duplicate").validate(),
            Err(BinanceError::DuplicateParameter)
        );
    }
    for value in ["0", "0.000", "-1", "+1", "1e-8", "0.1 ", ".1", "1."] {
        assert!(
            replace("amount", value).validate().is_err(),
            "amount={value:?}"
        );
    }
    for value in ["1", "SPOT", "MARGIN", ""] {
        assert_eq!(
            replace("walletType", value).validate(),
            Err(BinanceError::InvalidRequest)
        );
    }
    for key in [
        "path",
        "method",
        "transactionFeeFlag",
        "sideEffectType",
        "name",
        "memo",
    ] {
        assert_eq!(
            request.clone().push(key, "arbitrary").validate(),
            Err(BinanceError::UnsupportedParameter)
        );
    }
    let mut missing_network = request;
    missing_network
        .params
        .retain(|param| param.key != "network");
    assert_eq!(
        missing_network.validate(),
        Err(BinanceError::MissingParameter)
    );
}

#[test]
fn capital_network_preserves_optional_venue_address_and_memo_patterns() {
    let with_patterns: fin_binance::CapitalNetwork = parse_response(json!({
        "coin":"XRP", "network":"XRP", "addressRegex":"^r[1-9A-HJ-NP-Za-km-z]{25,34}$",
        "memoRegex":"^[0-9]{1,10}$", "withdrawTag":true
    }))
    .unwrap();
    assert_eq!(
        with_patterns.address_regex.as_deref(),
        Some("^r[1-9A-HJ-NP-Za-km-z]{25,34}$")
    );
    assert_eq!(with_patterns.memo_regex.as_deref(), Some("^[0-9]{1,10}$"));
    let without_patterns: fin_binance::CapitalNetwork =
        parse_response(json!({"coin":"BTC", "network":"BTC"})).unwrap();
    assert_eq!(without_patterns.address_regex, None);
    assert_eq!(without_patterns.memo_regex, None);
    assert!(
        parse_response::<fin_binance::CapitalNetwork>(
            json!({"coin":"BTC", "network":"BTC", "addressRegex":42})
        )
        .is_err()
    );
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
    let definition = fin_binance::operation_definitions()
        .into_iter()
        .find(|definition| definition.name == "binance.whitelist")
        .unwrap();
    assert_eq!(definition.kind, fin_domain::OperationKind::Read);
    assert_eq!(definition.semantics["editing_supported"], false);
}

#[test]
fn durable_request_preserves_exact_decimal_and_unescaped_email_strings() {
    let request = UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.000000000000000000123456"),
        from_email: None,
        to_email: Some("child+tag@example.invalid".into()),
        client_id: fin_binance::ClientIdentity::transfer("request-000001").unwrap(),
    }
    .request()
    .unwrap();
    let persisted = serde_json::to_value(&request).unwrap();
    let recovered: BinanceRequest = serde_json::from_value(persisted).unwrap();
    assert_eq!(recovered.get("amount"), Some("0.000000000000000000123456"));
    assert_eq!(recovered.get("toEmail"), Some("child+tag@example.invalid"));
    assert_eq!(recovered, request);
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
fn child_transfer_history_uses_fixed_typed_request_and_official_row_shape() {
    let request = fin_binance::subaccount_transfer_history_request("ETH", 2, 10, 20, 200).unwrap();
    assert_eq!(
        request.endpoint.method_path(),
        ("GET", "/sapi/v1/sub-account/transfer/subUserHistory")
    );
    assert_eq!(request.get("asset"), Some("ETH"));
    assert_eq!(request.get("type"), Some("2"));
    assert_eq!(request.get("startTime"), Some("10"));
    assert_eq!(request.get("endTime"), Some("20"));
    assert_eq!(request.get("limit"), Some("200"));
    assert_eq!(request.get("returnFailHistory"), Some("true"));
    assert!(
        fin_binance::BinanceRequest::new(fin_binance::BinanceEndpoint::SubAccountTransferHistory)
            .push("asset", "ETH")
            .push("type", "3")
            .push("startTime", "10")
            .push("endTime", "20")
            .push("limit", "1000")
            .push("returnFailHistory", "true")
            .validate()
            .is_err()
    );
    let rows: fin_binance::SubAccountTransferHistoryResponse =
        fin_binance::parse_response(json!([{
            "counterParty":"master", "email":"synthetic-master@example.invalid",
            "type":2,"asset":"ETH","qty":"0.05000000",
            "fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS",
            "tranId":123456789,"time":1000
        }]))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].quantity.checked_cmp(&amount("0.05")).is_eq());
    assert_eq!(rows[0].tran_id.as_str(), "123456789");
    assert!(
        fin_binance::parse_response::<fin_binance::SubAccountTransferHistoryResponse>(json!([
            {
                "counterParty":"master", "email":"synthetic-master@example.invalid",
                "type":"2","asset":"ETH","qty":"0.05",
                "fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS",
                "tranId":123456789,"time":1000
            }
        ]))
        .is_err()
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
    assert!(definitions.iter().all(|definition| match definition.kind {
        fin_domain::OperationKind::Read => {
            definition.execution_actor == fin_domain::ExecutionActor::AgentReadable
        }
        fin_domain::OperationKind::Write => {
            definition.execution_actor == fin_domain::ExecutionActor::AgentInvocable
        }
        fin_domain::OperationKind::Configuration | fin_domain::OperationKind::Lifecycle => false,
    }));
    for read in [
        "binance.balances",
        "binance.networks",
        "binance.deposit-address",
        "binance.deposit-addresses",
        "binance.spot.markets",
        "binance.futures.positions",
    ] {
        assert_eq!(
            definitions
                .iter()
                .find(|definition| definition.name == read)
                .unwrap_or_else(|| panic!("missing read definition {read}"))
                .execution_actor,
            fin_domain::ExecutionActor::AgentReadable,
            "generic plans must be able to include ordinary read steps"
        );
    }
    for (operation, field) in [
        ("binance.transfer", "/requestId"),
        ("binance.withdraw", "/requestId"),
        ("binance.spot-order", "/requestId"),
        ("binance.spot.order-place", "/request_id"),
        ("binance.futures.order-place", "/request_id"),
    ] {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == operation)
            .unwrap_or_else(|| panic!("missing write definition {operation}"));
        assert_eq!(
            definition.semantics["broker_generated_attempt_input_fields"],
            json!([field]),
            "only non-authority correlation IDs can vary by concrete attempt"
        );
    }
    for operation in ["binance.spot.order-cancel", "binance.futures.order-cancel"] {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == operation)
            .unwrap_or_else(|| panic!("missing write definition {operation}"));
        assert!(
            definition
                .semantics
                .get("broker_generated_attempt_input_fields")
                .is_none()
        );
    }
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
    binance_code: i64,
    timeout: bool,
    change_balances: bool,
    empty_history: bool,
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
        credential: &fin_binance::CredentialHandle,
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
            E::Account => {
                let is_child = credential.as_str().ends_with("child");
                Ok(json!({
                    "accountType":"SPOT",
                    "uid":if is_child {"1002"} else {"1001"},
                    "balances":[{
                        "asset":"ETH",
                        "free":if is_child {self.child_free.lock().unwrap().clone()} else {self.main_free.lock().unwrap().clone()},
                        "locked":"0"
                    }]
                }))
            }
            E::SubAccountTransferHistory => Ok(json!([])),
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
                    "status":"SUCCESS", "createTimeStamp":request.get("startTime").unwrap().parse::<u64>().unwrap()
                }]}))
            }
            endpoint => panic!("unexpected synthetic endpoint {endpoint:?}"),
        }
    }
}

#[derive(Clone, Copy)]
enum DefaultMasterReceiptFault {
    None,
    WrongCounterparty,
    WrongVenueId,
    WrongQuantity,
    WrongAsset,
    WrongType,
    WrongPeerEmail,
    WrongWallet,
    WrongStatus,
    OutsideWindow,
    Duplicate,
    FullPage,
    MasterHistoryIncomplete,
    WrongChildUid,
    ChildHistoryDenied,
    MissingChildReceipt,
    WrongMasterChildEmail,
}

struct DefaultMasterReceiptTransport {
    direction: TransferDirection,
    fault: DefaultMasterReceiptFault,
    main_free: std::sync::Mutex<String>,
    child_free: std::sync::Mutex<String>,
    client_id: std::sync::Mutex<Option<String>>,
    posts: std::sync::atomic::AtomicUsize,
    child_history_reads: std::sync::atomic::AtomicUsize,
    invalid_timestamp: std::sync::Mutex<Option<u64>>,
}

impl DefaultMasterReceiptTransport {
    fn new(direction: TransferDirection, fault: DefaultMasterReceiptFault) -> Self {
        let (main, child) = match direction {
            TransferDirection::MainToChild => ("1", "0"),
            TransferDirection::ChildToMain => ("0", "1"),
        };
        Self {
            direction,
            fault,
            main_free: std::sync::Mutex::new(main.into()),
            child_free: std::sync::Mutex::new(child.into()),
            client_id: std::sync::Mutex::new(None),
            posts: std::sync::atomic::AtomicUsize::new(0),
            child_history_reads: std::sync::atomic::AtomicUsize::new(0),
            invalid_timestamp: std::sync::Mutex::new(None),
        }
    }

    fn outside_timestamp(&self, end: u64) -> u64 {
        let mut timestamp = self.invalid_timestamp.lock().unwrap();
        // Master and child history reads capture different window end times.
        // A 1 ms offset can become valid before the child read on a busy runner.
        *timestamp.get_or_insert_with(|| end.saturating_add(86_400_000))
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for DefaultMasterReceiptTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        let child_credential = credential.as_str().ends_with("child");
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
                "asset":"ETH",
                "free":self.child_free.lock().unwrap().clone(),
                "locked":"0"
            }]})),
            E::Account => Ok(json!({
                "accountType":"SPOT",
                "uid":if child_credential {
                    if matches!(self.fault, DefaultMasterReceiptFault::WrongChildUid) {"1003"} else {"1002"}
                } else {"1001"},
                "balances":[{
                    "asset":"ETH",
                    "free":if child_credential {self.child_free.lock().unwrap().clone()} else {self.main_free.lock().unwrap().clone()},
                    "locked":"0"
                }]
            })),
            E::SubAccountTransferHistory => {
                self.child_history_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if matches!(self.fault, DefaultMasterReceiptFault::ChildHistoryDenied) {
                    return Err(fin_binance::TransportError::PermissionDenied);
                }
                // The pre-write capability probe has no associated transfer.
                if request.get("limit") == Some("1") {
                    return Ok(json!([]));
                }
                if matches!(self.fault, DefaultMasterReceiptFault::MissingChildReceipt) {
                    return Ok(json!([]));
                }
                let start = request.get("startTime").unwrap().parse::<u64>().unwrap();
                let end = request.get("endTime").unwrap().parse::<u64>().unwrap();
                assert_eq!(
                    start % 1_000,
                    0,
                    "child transfer history query uses Binance's second precision"
                );
                let time = if matches!(self.fault, DefaultMasterReceiptFault::OutsideWindow) {
                    self.outside_timestamp(end)
                } else {
                    start
                };
                let row = json!({
                    "counterParty":if matches!(self.fault, DefaultMasterReceiptFault::WrongCounterparty) {"another-sub"} else {"master"},
                    "email":if matches!(self.fault, DefaultMasterReceiptFault::WrongPeerEmail) {"wrong-master@example.invalid"} else {"synthetic-master@example.invalid"},
                    "type":if matches!(self.fault, DefaultMasterReceiptFault::WrongType) {
                        if request.get("type") == Some("1") {2} else {1}
                    } else {request.get("type").unwrap().parse::<u64>().unwrap()},
                    "asset":if matches!(self.fault, DefaultMasterReceiptFault::WrongAsset) {"BTC"} else {"ETH"},
                    "qty":if matches!(self.fault, DefaultMasterReceiptFault::WrongQuantity) {"0.24"} else {"0.25000000"},
                    "fromAccountType":if matches!(self.fault, DefaultMasterReceiptFault::WrongWallet) {"MARGIN"} else {"SPOT"},
                    "toAccountType":"SPOT",
                    "status":if matches!(self.fault, DefaultMasterReceiptFault::WrongStatus) {"PENDING"} else {"SUCCESS"},
                    "tranId":if matches!(self.fault, DefaultMasterReceiptFault::WrongVenueId) {123456790u64} else {123456789u64},
                    "time":time
                });
                if matches!(self.fault, DefaultMasterReceiptFault::Duplicate) {
                    Ok(json!([row.clone(), row]))
                } else if matches!(self.fault, DefaultMasterReceiptFault::FullPage) {
                    Ok(json!(vec![row; 200]))
                } else {
                    Ok(json!([row]))
                }
            }
            E::UniversalTransfer => {
                let previous_posts = self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.client_id.lock().unwrap() = request.get("clientTranId").map(str::to_owned);
                let child_sends = request.get("fromEmail").is_some();
                if previous_posts == 0 {
                    match (self.direction, child_sends) {
                        (TransferDirection::ChildToMain, true) => {
                            *self.main_free.lock().unwrap() = "0.25".into();
                            *self.child_free.lock().unwrap() = "0.75".into();
                        }
                        (TransferDirection::MainToChild, false) => {
                            *self.main_free.lock().unwrap() = "0.75".into();
                            *self.child_free.lock().unwrap() = "0.25".into();
                        }
                        _ => return Err(fin_binance::TransportError::Rejected),
                    }
                } else if child_sends {
                    *self.main_free.lock().unwrap() = "1".into();
                    *self.child_free.lock().unwrap() = "0".into();
                } else {
                    *self.main_free.lock().unwrap() = "0.75".into();
                    *self.child_free.lock().unwrap() = "0.25".into();
                }
                Err(fin_binance::TransportError::Timeout)
            }
            E::UniversalTransferHistory => {
                let id = self.client_id.lock().unwrap().clone().unwrap_or_default();
                let peer_email = if matches!(self.fault, DefaultMasterReceiptFault::WrongPeerEmail)
                {
                    "different-peer@example.invalid"
                } else {
                    "synthetic-master@example.invalid"
                };
                let child_email =
                    if matches!(self.fault, DefaultMasterReceiptFault::WrongMasterChildEmail) {
                        "wrong-child@example.invalid"
                    } else {
                        "child@example.invalid"
                    };
                let child_sends = request.get("fromEmail").is_some();
                let (from_email, to_email) = if child_sends {
                    (child_email, peer_email)
                } else {
                    (peer_email, child_email)
                };
                let end = request.get("endTime").unwrap().parse::<u64>().unwrap();
                let start = request.get("startTime").unwrap().parse::<u64>().unwrap();
                assert_eq!(
                    start % 1_000,
                    0,
                    "universal-transfer history query uses Binance's second precision"
                );
                let time = if matches!(self.fault, DefaultMasterReceiptFault::OutsideWindow) {
                    self.outside_timestamp(end)
                } else {
                    start
                };
                let row = json!({
                    "tranId":123456789,
                    "clientTranId":id,
                    "asset":"ETH", "amount":"0.25", "fromAccountType":"SPOT",
                    "toAccountType":"SPOT", "fromEmail":from_email, "toEmail":to_email,
                    "status":"SUCCESS", "createTimeStamp":time
                });
                if matches!(
                    self.fault,
                    DefaultMasterReceiptFault::MasterHistoryIncomplete
                ) {
                    let mut result = vec![row];
                    result.extend((0..499).map(|index| {
                        json!({
                            "tranId":format!("noise-{index}"),"clientTranId":"other-request",
                            "asset":"ETH","amount":"0.25","fromAccountType":"SPOT",
                            "toAccountType":"SPOT","fromEmail":from_email,"toEmail":to_email,
                            "status":"SUCCESS","createTimeStamp":time
                        })
                    }));
                    Ok(json!({"totalCount":12000,"result":result}))
                } else {
                    Ok(json!({"totalCount":1,"result":[row]}))
                }
            }
            endpoint => panic!("unexpected synthetic endpoint {endpoint:?}"),
        }
    }
}

async fn submit_and_reconcile_default_master_transfer(
    transport: std::sync::Arc<DefaultMasterReceiptTransport>,
    from: &str,
    to: &str,
    request_id: &str,
    legacy_submission: bool,
) -> (fin_domain::Reconciliation, fin_domain::Submission) {
    use fin_domain::Adapter;
    let config = main_and_child_config();
    let adapter =
        fin_binance::BinanceAdapter::new(config.clone(), transport.clone(), None, None).unwrap();
    let preview = adapter
        .prepare(
            "binance.transfer",
            &json!({
                "requestId":request_id, "authority":"main-fixture",
                "from":from, "to":to, "coin":"ETH", "amount":"0.25",
                "fromWallet":"SPOT", "toWallet":"SPOT"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        preview.binding["params"]["params"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|param| param["key"] == "fromEmail" || param["key"] == "toEmail")
            .count(),
        1
    );
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
    let mut original = journal.persisted.lock().unwrap()[0].clone();
    let submitted_second = chrono::Utc::now().timestamp_millis().div_euclid(1_000) * 1_000;
    let submitted_at = chrono::DateTime::from_timestamp_millis(submitted_second + 605).unwrap();
    original.data["submittedAt"] = json!(submitted_at.to_rfc3339());
    if legacy_submission {
        original
            .data
            .as_object_mut()
            .unwrap()
            .remove("historySelector");
    }
    assert_eq!(original.reference, preview.binding["clientTranId"]);
    assert!(original.data["toEmail"].is_null() || original.data["fromEmail"].is_null());
    let record = fin_domain::OperationRecord {
        id: format!("restart-{request_id}"),
        request_id: request_id.to_owned(),
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
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    // Recreate the adapter to model restart. Reconciliation remains read-only.
    let restarted = fin_binance::BinanceAdapter::new(config, transport, None, None).unwrap();
    let result = restarted.reconcile(&record).await.unwrap();
    (result, original)
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
            if self.timeout {
                return Err(fin_binance::TransportError::Timeout);
            }
            return Err(fin_binance::TransportError::HttpFailure {
                status: 400,
                binance_code: Some(self.binance_code),
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
            if self.empty_history {
                return Ok(json!({"totalCount":0,"result":[]}));
            }
            if self.binance_code == -1022 {
                return Ok(json!({"totalCount":0,"result":[]}));
            }
            if read == 0 {
                return Ok(json!({"totalCount":1,"result":[{
                    "tranId":123456789,
                    "clientTranId":self.client_id.lock().unwrap().clone(),
                    "asset":"ETH", "amount":"0.25", "fromAccountType":"SPOT",
                    "toAccountType":"SPOT", "fromEmail":"child@example.invalid",
                    "status":"PENDING", "createTimeStamp":request.get("startTime").unwrap().parse::<u64>().unwrap()
                }]}));
            }
            return Ok(json!({"totalCount":0,"result":[]}));
        }
        if self.change_balances
            && self.posts.load(std::sync::atomic::Ordering::SeqCst) > 0
            && request.endpoint == BinanceEndpoint::Account
            && !credential.as_str().ends_with("child")
        {
            return Ok(
                json!({"accountType":"SPOT","uid":"1001","balances":[{"asset":"ETH","free":"0.25","locked":"0"}]}),
            );
        }
        fin_binance::BinanceTransport::request(&self.inner, credential, request).await
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for TransferFixtureTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
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
            E::Account => {
                let is_child = credential.as_str().ends_with("child");
                Ok(json!({
                    "accountType":"SPOT",
                    "uid":if is_child {"1002"} else {"1001"},
                    "balances":[{"asset":"ETH", "free":"0", "locked":"0"}]
                }))
            }
            E::SubAccountTransferHistory => Ok(json!([])),
            E::UniversalTransferHistory => Ok(json!({"totalCount":1,"result":[{
                "tranId":123456789,
                "clientTranId":request.get("clientTranId"),
                "asset":"ETH", "amount":"0.123456", "fromAccountType":"SPOT",
                "toAccountType":"SPOT", "fromEmail":"child@example.invalid",
                "status":self.transfer_history_status, "createTimeStamp":request.get("startTime").unwrap().parse::<u64>().unwrap()
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
        dispatch_evidence: None,
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
            dispatch_evidence: None,
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

#[tokio::test]
async fn default_master_email_recovers_forward_and_return_from_child_history() {
    let transport = std::sync::Arc::new(DefaultMasterReceiptTransport::new(
        TransferDirection::MainToChild,
        DefaultMasterReceiptFault::None,
    ));
    let (forward, forward_submission) = submit_and_reconcile_default_master_transfer(
        transport.clone(),
        "main-fixture",
        "child-fixture",
        "synthetic-default-master-forward",
        false,
    )
    .await;
    assert_eq!(forward.status, fin_domain::Status::Completed);
    let forward_proof = forward.verified.unwrap();
    assert_eq!(forward_proof.submission, forward_submission);
    assert_eq!(
        forward_proof.evidence["transfer"]["childHistoryVerified"],
        true
    );
    assert_eq!(
        forward_proof.evidence["transfer"]["childHistoryProof"]["authenticatedUid"],
        "1002"
    );
    assert_eq!(
        forward_proof.evidence["transfer"]["childHistoryProof"]["counterParty"],
        "master"
    );
    assert_eq!(
        forward_proof.evidence["transfer"]["childHistoryProof"]["directionType"],
        1
    );
    assert_eq!(
        forward_proof.evidence["transfer"]["childHistoryProof"]["quantity"],
        "0.25"
    );

    // The same synthetic account pair now returns the funded quantity. This
    // record models a pre-history-selector durable submission after restart;
    // no original identity or route email is rewritten.
    let (return_leg, return_submission) = submit_and_reconcile_default_master_transfer(
        transport.clone(),
        "child-fixture",
        "main-fixture",
        "synthetic-default-master-return",
        true,
    )
    .await;
    assert_eq!(return_leg.status, fin_domain::Status::Completed);
    let return_proof = return_leg.verified.unwrap();
    assert_eq!(return_proof.submission, return_submission);
    assert_eq!(return_proof.submission.data["historySelector"], Value::Null);
    assert_eq!(
        return_proof.evidence["transfer"]["childHistoryVerified"],
        true
    );
    assert_eq!(
        return_proof.evidence["transfer"]["childHistoryProof"]["directionType"],
        2
    );
    assert_eq!(
        return_proof.evidence["transfer"]["childHistoryProof"]["timestampWithinSubmissionWindow"],
        true
    );
    assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn default_master_peer_proof_refuses_mismatch_duplicates_caps_and_bad_child_identity() {
    use fin_domain::Adapter;
    let invalid_receipts = [
        DefaultMasterReceiptFault::WrongCounterparty,
        DefaultMasterReceiptFault::WrongVenueId,
        DefaultMasterReceiptFault::WrongQuantity,
        DefaultMasterReceiptFault::WrongAsset,
        DefaultMasterReceiptFault::WrongType,
        DefaultMasterReceiptFault::WrongPeerEmail,
        DefaultMasterReceiptFault::WrongWallet,
        DefaultMasterReceiptFault::WrongStatus,
        DefaultMasterReceiptFault::OutsideWindow,
        DefaultMasterReceiptFault::Duplicate,
        DefaultMasterReceiptFault::FullPage,
        DefaultMasterReceiptFault::MissingChildReceipt,
        DefaultMasterReceiptFault::WrongMasterChildEmail,
        DefaultMasterReceiptFault::MasterHistoryIncomplete,
    ];
    for (index, fault) in invalid_receipts.into_iter().enumerate() {
        let transport = std::sync::Arc::new(DefaultMasterReceiptTransport::new(
            TransferDirection::ChildToMain,
            fault,
        ));
        let (result, original) = submit_and_reconcile_default_master_transfer(
            transport.clone(),
            "child-fixture",
            "main-fixture",
            &format!("synthetic-invalid-peer-{index}"),
            false,
        )
        .await;
        assert_eq!(
            result.status,
            fin_domain::Status::Unknown,
            "fault index {index}"
        );
        assert!(result.verified.is_none(), "fault index {index}");
        assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(original.data["toEmail"], Value::Null);
    }

    for fault in [
        DefaultMasterReceiptFault::WrongChildUid,
        DefaultMasterReceiptFault::ChildHistoryDenied,
    ] {
        let transport = std::sync::Arc::new(DefaultMasterReceiptTransport::new(
            TransferDirection::ChildToMain,
            fault,
        ));
        let adapter = fin_binance::BinanceAdapter::new(
            main_and_child_config(),
            transport.clone(),
            None,
            None,
        )
        .unwrap();
        let preview = adapter
            .prepare(
                "binance.transfer",
                &json!({
                    "requestId":"synthetic-child-proof-readiness", "authority":"main-fixture",
                    "from":"child-fixture", "to":"main-fixture", "coin":"ETH",
                    "amount":"0.25", "fromWallet":"SPOT", "toWallet":"SPOT"
                }),
            )
            .await;
        assert!(preview.is_err());
        assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 0);
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
        binance_code: -2010,
        timeout: false,
        change_balances: false,
        empty_history: false,
    });
    let adapter = std::sync::Arc::new(
        fin_binance::BinanceAdapter::new(
            main_and_child_config(),
            transport.clone(),
            Some(std::sync::Arc::new(DiagnosticPrices)),
            None,
        )
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

#[tokio::test]
async fn exact_signature_rejection_releases_zero_effect_transfer_but_timeout_and_balance_changes_stay_unknown()
 {
    use fin_domain::Adapter;
    let make = |binance_code, timeout, change_balances, empty_history| {
        std::sync::Arc::new(DiagnosticTransferTransport {
            inner: RouteAwareTransferTransport::new(TransferDirection::ChildToMain),
            posts: std::sync::atomic::AtomicUsize::new(0),
            history_reads: std::sync::atomic::AtomicUsize::new(0),
            client_id: std::sync::Mutex::new(None),
            binance_code,
            timeout,
            change_balances,
            empty_history,
        })
    };
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-signature-rejection".into(),
        operation: "binance.transfer".into(),
        input: json!({
            "requestId":"synthetic-signature-rejection", "authority":"main-fixture",
            "from":"child-fixture", "to":"main-fixture", "coin":"ETH",
            "amount":"0.25", "fromWallet":"SPOT", "toWallet":"SPOT"
        }),
        run_id: None,
    };
    let transport = make(-1022, false, false, true);
    let adapter = std::sync::Arc::new(
        fin_binance::BinanceAdapter::new(
            main_and_child_config(),
            transport.clone(),
            Some(std::sync::Arc::new(DiagnosticPrices)),
            None,
        )
        .unwrap(),
    );
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![adapter.clone()],
        std::sync::Arc::new(SyntheticApproval(std::sync::atomic::AtomicUsize::new(0))),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let initial = engine.execute(request.clone()).await.unwrap();
    assert_eq!(initial.status, fin_domain::Status::Unknown);
    assert!(initial.error.as_ref().unwrap().details.is_some());
    let failed = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(failed.status, fin_domain::Status::Failed);
    assert!(
        failed
            .effects
            .as_ref()
            .is_some_and(|effects| effects.effects.is_empty())
    );
    assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let duplicate = engine.execute(request.clone()).await.unwrap();
    assert_eq!(duplicate.status, fin_domain::Status::Failed);
    assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Pre-structured durable errors used the exact sanitized broker message.
    // Reconciliation retains compatibility with that immutable representation.
    let mut legacy = initial.clone();
    legacy.error.as_mut().unwrap().details = None;
    let legacy_result = adapter.reconcile(&legacy).await.unwrap();
    assert_eq!(legacy_result.status, fin_domain::Status::Failed);
    assert!(legacy_result.verified.unwrap().effects.is_empty());

    for (suffix, code, timeout, changed, empty_history) in [
        ("timeout", -1007, true, false, true),
        ("non-signature", -2010, false, false, true),
        ("changed-balances", -1022, false, true, true),
    ] {
        let transport = make(code, timeout, changed, empty_history);
        let adapter = std::sync::Arc::new(
            fin_binance::BinanceAdapter::new(
                main_and_child_config(),
                transport.clone(),
                Some(std::sync::Arc::new(DiagnosticPrices)),
                None,
            )
            .unwrap(),
        );
        let engine = fin_engine::Engine::new(
            fin_store::Store::in_memory().unwrap(),
            vec![adapter],
            std::sync::Arc::new(SyntheticApproval(std::sync::atomic::AtomicUsize::new(0))),
            fin_engine::EngineOptions::default(),
        )
        .unwrap();
        let mut request = request.clone();
        request.request_id = format!("synthetic-signature-{suffix}");
        request.input["requestId"] = json!(request.request_id);
        let initial = engine.execute(request).await.unwrap();
        let result = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(result.status, fin_domain::Status::Unknown, "{suffix}");
        assert!(result.effects.is_none(), "{suffix}");
        assert_eq!(
            transport.posts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{suffix}"
        );
    }
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
            E::ApiRestrictions => {
                json!({"enableReading":true,"enableSpotAndMarginTrading":false,"enableFutures":false})
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
            E::ApiRestrictions => Ok(
                json!({"enableReading":true,"enableSpotAndMarginTrading":false,"enableFutures":false}),
            ),
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
async fn child_authority_cannot_sign_master_to_child_without_main_credential_substitution() {
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
                "from":"main-fixture","to":"child-fixture","coin":"ETH",
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
        fin_domain::CostTreatment::IncludedInDebit
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

    let exact = preview_with_free("0.01").await.unwrap();
    assert_eq!(exact.intent.debits[0].amount.atoms.to_decimal(36), "0.01");
    assert_eq!(
        exact.intent.maximum_costs[0].amount.atoms.to_decimal(36),
        "0.001"
    );
    assert!(
        preview_with_free("0.009999999999999999999999999999999999")
            .await
            .is_err()
    );
}

struct SyntheticCredentials;

impl fin_exchange::ccxt::CredentialProvider for SyntheticCredentials {
    fn credentials(
        &self,
        reference: &str,
    ) -> std::result::Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError> {
        assert!(reference.starts_with("test-only-"));
        Ok(fin_exchange::ccxt::Credentials {
            api_key: "synthetic-test-api-key".to_owned().into(),
            secret: "synthetic-fixture-hmac-secret-not-a-real-key"
                .to_owned()
                .into(),
        })
    }
}

fn fixture_ccxt_transport(address: std::net::SocketAddr) -> fin_binance::BinanceCcxtTransport {
    let client = fin_exchange::ccxt::CcxtClient::for_test(&format!("http://{address}")).unwrap();
    fin_binance::BinanceCcxtTransport::new(
        std::sync::Arc::new(client),
        std::sync::Arc::new(SyntheticCredentials),
    )
}

async fn read_fixture_http_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "synthetic request closed before its body arrived");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let body_len = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + body_len {
            return String::from_utf8(request).unwrap();
        }
    }
}

fn percent_decode_component(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap();
            decoded.push(u8::from_str_radix(hex, 16).unwrap());
            index += 3;
        } else {
            decoded.push(if bytes[index] == b'+' {
                b' '
            } else {
                bytes[index]
            });
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap()
}

fn request_parameters(wire: &str) -> std::collections::BTreeMap<String, String> {
    let (headers, body) = wire.split_once("\r\n\r\n").unwrap();
    let target = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    query
        .split('&')
        .chain(body.split('&'))
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .expect("CCXT must percent-encode each form parameter before signing");
            (
                percent_decode_component(key),
                percent_decode_component(value),
            )
        })
        .collect()
}

#[tokio::test]
async fn ccxt_transport_keeps_exact_parameters_closed_endpoint_and_large_integer_ids() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_fixture_http_request(&mut stream).await;
        let body = r#"{"tranId":9007199254740993}"#;
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
        ).as_bytes()).await.unwrap();
        request
    });
    let transport = fixture_ccxt_transport(address);
    let client_id = fin_binance::ClientIdentity::transfer("http-fixture-1").unwrap();
    let request = fin_binance::UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.000000000000000000123456"),
        from_email: None,
        to_email: Some("name@plus+amp&pct%雪.example".into()),
        client_id: client_id.clone(),
    }
    .request()
    .unwrap();
    let journal = std::sync::Arc::new(RecordingJournal {
        events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let response = fin_binance::BinanceTransport::request_authorized(
        &transport,
        &fin_binance::CredentialHandle::new("test-only-http-credential").unwrap(),
        request,
        journal,
    )
    .await
    .unwrap();
    assert_eq!(response["tranId"].as_u64(), Some(9_007_199_254_740_993));
    let wire = server.await.unwrap();
    let first_line = wire.lines().next().unwrap();
    let mut parts = first_line.split_whitespace();
    assert_eq!(parts.next(), Some("POST"));
    assert_eq!(
        parts.next().unwrap().split('?').next(),
        Some("/sapi/v1/sub-account/universalTransfer")
    );
    assert!(
        wire.to_ascii_lowercase()
            .contains("x-mbx-apikey: synthetic-test-api-key")
    );
    let params = request_parameters(&wire);
    assert_eq!(params["amount"], "0.000000000000000000123456");
    assert_eq!(params["toEmail"], "name@plus+amp&pct%雪.example");
    assert_eq!(params["asset"], "ETH");
    assert_eq!(params["fromAccountType"], "SPOT");
    assert_eq!(params["toAccountType"], "SPOT");
    assert_eq!(params["clientTranId"], client_id.as_str());
    assert_eq!(params["signature"].len(), 64);
    assert!(params["timestamp"].parse::<u64>().is_ok());
    assert!(!wire.contains("synthetic-fixture-hmac-secret-not-a-real-key"));
}

#[tokio::test]
async fn universal_transfer_transport_ed25519_signs_encoded_route_email_exactly() {
    use base64::Engine as _;
    use ed25519_dalek::{Verifier, pkcs8::EncodePrivateKey};
    use tokio::io::AsyncWriteExt;

    struct Ed25519Credentials(String);
    impl fin_exchange::ccxt::CredentialProvider for Ed25519Credentials {
        fn credentials(
            &self,
            reference: &str,
        ) -> std::result::Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError>
        {
            assert_eq!(reference, "test-only-ed25519-transfer");
            Ok(fin_exchange::ccxt::Credentials {
                api_key: "synthetic-ed25519-api-key".to_owned().into(),
                secret: self.0.clone().into(),
            })
        }
    }

    let key = ed25519_dalek::SigningKey::from_bytes(&[37; 32]);
    let private_key = key.to_pkcs8_pem(Default::default()).unwrap().to_string();
    let email = "synthetic+route&percent%雪@example.invalid";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let wire = read_fixture_http_request(&mut stream).await;
        let body = r#"{"tranId":9007199254740993}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        wire
    });
    let transport = fin_binance::BinanceCcxtTransport::new(
        std::sync::Arc::new(
            fin_exchange::ccxt::CcxtClient::for_test(&format!("http://{address}")).unwrap(),
        ),
        std::sync::Arc::new(Ed25519Credentials(private_key)),
    );
    let intent = fin_binance::UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.01265"),
        from_email: None,
        to_email: Some(email.into()),
        client_id: fin_binance::ClientIdentity::transfer("synthetic-ed25519-transfer-1").unwrap(),
    };
    let journal = std::sync::Arc::new(RecordingJournal {
        events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let response = fin_binance::BinanceTransport::request_authorized(
        &transport,
        &fin_binance::CredentialHandle::new("test-only-ed25519-transfer").unwrap(),
        intent.request().unwrap(),
        journal,
    )
    .await
    .unwrap();
    assert_eq!(response["tranId"].as_u64(), Some(9_007_199_254_740_993));

    let wire = server.await.unwrap();
    assert!(wire.starts_with("POST /sapi/v1/sub-account/universalTransfer?"));
    assert!(
        wire.to_ascii_lowercase()
            .contains("x-mbx-apikey: synthetic-ed25519-api-key")
    );
    let params = request_parameters(&wire);
    assert_eq!(params["toEmail"], email);
    assert_eq!(params["amount"], "0.01265");
    let target = wire
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let query = target.split_once('?').unwrap().1;
    let (payload, signature) = query.rsplit_once("&signature=").unwrap();
    assert!(payload.contains("toEmail=synthetic%2Broute%26percent%25%E9%9B%AA%40example.invalid"));
    assert!(!payload.contains("%2540"));
    let signature = percent_decode_component(signature);
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .unwrap();
    key.verifying_key()
        .verify(
            payload.as_bytes(),
            &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
        )
        .unwrap();
    let unencoded_payload = payload.replace(
        "synthetic%2Broute%26percent%25%E9%9B%AA%40example.invalid",
        email,
    );
    assert!(
        key.verifying_key()
            .verify(
                unencoded_payload.as_bytes(),
                &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
            )
            .is_err()
    );
    assert!(!wire.contains("-----BEGIN PRIVATE KEY-----"));
}

#[tokio::test]
async fn ccxt_withdrawal_encodes_utf8_route_address_and_memo_as_individual_parameters() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_fixture_http_request(&mut stream).await;
        let body = r#"{"id":"synthetic-withdrawal-id"}"#;
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
        ).as_bytes()).await.unwrap();
        request
    });
    let transport = fixture_ccxt_transport(address);
    let client_id = fin_binance::ClientIdentity::withdrawal("encoded-withdrawal-000001").unwrap();
    let request = StandardWithdrawalIntent {
        coin: "币".into(),
        network: "网络".into(),
        amount: amount("0.000000000000000000123456"),
        address: "synthetic+&%雪?=address".into(),
        memo: Some("memo 雪+&%=tag".into()),
        client_id: client_id.clone(),
    }
    .request()
    .unwrap();
    let response = fin_binance::BinanceTransport::request(
        &transport,
        &fin_binance::CredentialHandle::new("test-only-http-credential").unwrap(),
        request,
    )
    .await
    .unwrap();
    assert_eq!(response["id"], "synthetic-withdrawal-id");
    let wire = server.await.unwrap();
    let first_line = wire.lines().next().unwrap();
    let mut parts = first_line.split_whitespace();
    assert_eq!(parts.next(), Some("POST"));
    assert_eq!(
        parts.next().unwrap().split('?').next(),
        Some("/sapi/v1/capital/withdraw/apply")
    );
    let params = request_parameters(&wire);
    assert_eq!(params["coin"], "币");
    assert_eq!(params["network"], "网络");
    assert_eq!(params["address"], "synthetic+&%雪?=address");
    assert_eq!(params["addressTag"], "memo 雪+&%=tag");
    assert_eq!(params["amount"], "0.000000000000000000123456");
    assert_eq!(params["walletType"], "0");
    assert_eq!(params["withdrawOrderId"], client_id.as_str());
    assert_eq!(params["signature"].len(), 64);
    assert!(params["timestamp"].parse::<u64>().is_ok());
    assert_eq!(
        params
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "coin",
            "network",
            "address",
            "addressTag",
            "amount",
            "walletType",
            "withdrawOrderId",
            "recvWindow",
            "timestamp",
            "signature"
        ]
        .into_iter()
        .collect(),
        "encoded delimiters cannot create extra parameters"
    );
    assert!(matches!(
        params["recvWindow"].parse::<u64>(),
        Ok(1..=60_000)
    ));
    assert!(!wire.contains("synthetic-fixture-hmac-secret-not-a-real-key"));
}

#[tokio::test]
async fn loopback_http_rate_limits_are_reported_once_without_retry_for_418_or_429() {
    use tokio::io::AsyncWriteExt;
    for status in [418u16, 429u16] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut stream).await;
            assert!(request.starts_with("POST /sapi/v1/sub-account/universalTransfer"));
            stream.write_all(format!(
                "HTTP/1.1 {status} Rate Limited\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ).as_bytes()).await.unwrap();
            drop(stream);
            tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                .await
                .is_ok()
        });
        let transport = fixture_ccxt_transport(address);
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
        let journal = std::sync::Arc::new(RecordingJournal {
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            persisted: std::sync::Mutex::new(Vec::new()),
        });
        let result = fin_binance::BinanceTransport::request_authorized(
            &transport,
            &fin_binance::CredentialHandle::new("test-only-rate-limit").unwrap(),
            request,
            journal,
        )
        .await;
        assert!(
            matches!(result, Err(fin_binance::TransportError::HttpFailure {
            status: response_status, binance_code: None,
            kind: fin_binance::HttpFailureKind::RateLimited,
        }) if response_status == status),
            "unexpected rate-limit result: {result:?}"
        );
        assert!(
            !server.await.unwrap(),
            "HTTP {status} caused an automatic second request"
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
        let transport = fixture_ccxt_transport(address);
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
    coin: String,
    network: String,
    api_amount: String,
    history_amount: String,
    transaction_fee: String,
    source_free_before: String,
    source_free_after: String,
    published_fee: String,
    contract_address: Option<String>,
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
                "coin":self.coin, "withdrawAllEnable":true, "depositAllEnable":true,
            "networkList":[{
                "network":self.network, "coin":self.coin, "depositEnable":true,
                    "withdrawEnable":true, "withdrawIntegerMultiple":"0.00000001",
                    "withdrawFee":self.published_fee, "withdrawMin":"0.001", "withdrawMax":"1000",
                    "contractAddress":self.contract_address,
                    "withdrawTag":false, "busy":false
                }]
            }])),
            E::Account => {
                let read = self
                    .account_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let free = if read < 3 {
                    self.source_free_before.as_str()
                } else {
                    self.source_free_after.as_str()
                };
                Ok(json!({"balances":[{"asset":self.coin,"free":free,"locked":"0"}]}))
            }
            E::StandardWithdrawal => {
                assert_eq!(request.get("network"), Some(self.network.as_str()));
                assert_eq!(request.get("walletType"), Some("0"));
                assert_eq!(request.get("coin"), Some(self.coin.as_str()));
                assert_eq!(request.get("amount"), Some(self.api_amount.as_str()));
                if self.timeout_withdrawal {
                    Err(fin_binance::TransportError::Timeout)
                } else {
                    Ok(json!({"id":"withdraw-fixture-01"}))
                }
            }
            E::WithdrawalHistory => Ok(json!([{
                "id":"withdraw-fixture-01", "coin":self.coin, "amount":self.history_amount,
                "transactionFee":self.transaction_fee, "network":self.network,
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

struct CompletedRecipientVerifier {
    asset: fin_domain::AssetId,
    amount_atoms: Option<fin_domain::Quantity>,
    chain_id: u64,
    status: fin_domain::Status,
    evidence: Value,
}

#[async_trait::async_trait]
impl fin_binance::BinanceRecipientVerifier for CompletedRecipientVerifier {
    async fn verify(&self, request: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        assert_eq!(request.chain_id, self.chain_id);
        assert_eq!(
            request.recipient,
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(request.asset, self.asset);
        fin_binance::PayoutResult {
            status: self.status.clone(),
            amount_atoms: self.amount_atoms.clone(),
            evidence: self.evidence.clone(),
        }
    }
}

fn withdrawal_destination(coin: &str, asset: fin_domain::AssetId) -> fin_binance::BinanceConfig {
    let mut config = one_main_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "wallet-fixture".into(),
        resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        assets: std::collections::BTreeMap::from([(coin.to_owned(), asset)]),
        confirmations: 2,
    });
    config
}

async fn reconcile_eth_withdrawal_fixture(
    request_id: &str,
    gross: &str,
    history_net: &str,
    fee: &str,
    source_free_after: &str,
    maximum_fee: &str,
    minimum_receive: &str,
    receipt_status: fin_domain::Status,
    receipt_atoms: Option<&str>,
    legacy_minimum_receive: Option<&str>,
) -> fin_domain::Result<fin_domain::Reconciliation> {
    use fin_domain::Adapter;
    let asset = fin_domain::AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        withdrawal_destination("ETH", asset.clone()),
        std::sync::Arc::new(WithdrawalLifecycleTransport {
            account_reads: std::sync::atomic::AtomicUsize::new(0),
            events: events.clone(),
            timeout_withdrawal: false,
            coin: "ETH".into(),
            network: "ETH".into(),
            api_amount: gross.into(),
            history_amount: history_net.into(),
            transaction_fee: fee.into(),
            source_free_before: "1.2345".into(),
            source_free_after: source_free_after.into(),
            published_fee: maximum_fee.into(),
            contract_address: None,
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier {
            asset,
            amount_atoms: receipt_atoms.map(fin_domain::Quantity::new).transpose()?,
            chain_id: 1,
            status: receipt_status,
            evidence: json!({"fixture":"synthetic selected-chain receipt"}),
        })),
    )?;
    let input = json!({
        "requestId":request_id, "account":"main-fixture", "destination":"wallet-fixture",
        "coin":"ETH", "amount":gross, "network":"ETH", "walletType":"SPOT",
        "maxFee":maximum_fee, "minReceiveAmount":minimum_receive
    });
    let mut preview = adapter.prepare("binance.withdraw", &input).await?;
    let mut persisted_input = input;
    let journal = std::sync::Arc::new(RecordingJournal {
        events,
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let mut submission = adapter.submit(&preview, journal).await?;
    if let Some(old_floor) = legacy_minimum_receive {
        // Simulate a saved preview approved by an older build. Its immutable
        // minimum was gross amount even though Binance later paid gross-fee.
        preview.binding["minimumReceive"] = json!(old_floor);
        preview.input["minReceiveAmount"] = json!(old_floor);
        persisted_input["minReceiveAmount"] = json!(old_floor);
        submission.data["binding"]["minimumReceive"] = json!(old_floor);
        submission.data["minimumReceive"] = json!(old_floor);
        let atoms = fin_domain::Quantity::from_decimal(old_floor, 18)?;
        for constraint in &mut preview.intent.expected {
            if let fin_domain::EffectConstraint::Transfer { minimum_credit, .. } = constraint {
                *minimum_credit = atoms.clone();
            }
        }
        for cost in &mut preview.intent.maximum_costs {
            cost.treatment = fin_domain::CostTreatment::AdditionalDebit;
        }
    }
    let record = fin_domain::OperationRecord {
        id: format!("{request_id}-operation"),
        request_id: request_id.into(),
        fingerprint: format!("{request_id}-fingerprint"),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input: persisted_input,
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    adapter.reconcile(&record).await
}

#[tokio::test]
async fn withdrawal_lifecycle_correlates_order_fee_and_cross_asset_chain_receipt() {
    use fin_domain::Adapter;
    let asset = fin_domain::AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let config = withdrawal_destination("ETH", asset.clone());
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(WithdrawalLifecycleTransport {
            account_reads: std::sync::atomic::AtomicUsize::new(0),
            events: events.clone(),
            timeout_withdrawal: false,
            coin: "ETH".into(),
            network: "ETH".into(),
            api_amount: "0.0125".into(),
            history_amount: "0.01235".into(),
            transaction_fee: "0.00015".into(),
            source_free_before: "1.2345".into(),
            source_free_after: "1.2220".into(),
            published_fee: "0.00015".into(),
            contract_address: None,
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier {
            asset,
            amount_atoms: Some(fin_domain::Quantity::new("12350000000000000").unwrap()),
            chain_id: 1,
            status: fin_domain::Status::Completed,
            evidence: json!({"fixture":"exact synthetic Ethereum receipt"}),
        })),
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-withdraw-lifecycle", "account":"main-fixture",
        "destination":"wallet-fixture", "coin":"ETH", "amount":"0.0125",
        "network":"ETH", "walletType":"SPOT", "maxFee":"0.00015",
        "minReceiveAmount":"0.01235"
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
        dispatch_evidence: None,
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
            assert_eq!(debit.atoms.to_decimal(36), "0.0125");
            assert_eq!(credit.asset.network, "ethereum:1");
            assert_eq!(credit.asset.decimals, 18);
            assert_eq!(credit.atoms.to_decimal(18), "0.01235");
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
            assert_eq!(amount.atoms.to_decimal(36), "0.00015");
            assert_eq!(treatment, &fin_domain::CostTreatment::IncludedInDebit);
        }
        other => panic!("unexpected withdrawal cost effect {other:?}"),
    }
    assert_eq!(proof.evidence["sourceDebitObserved"], "0.0125");
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

#[tokio::test]
async fn usdc_withdrawal_gross_api_amount_net_history_and_fee_are_accounted_once() {
    use fin_domain::Adapter;
    let asset = fin_domain::AssetId {
        network: "ethereum:1".into(),
        reference: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
        decimals: 6,
    };
    let config = withdrawal_destination("USDC", asset.clone());
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(WithdrawalLifecycleTransport {
            account_reads: std::sync::atomic::AtomicUsize::new(0),
            events: events.clone(),
            timeout_withdrawal: false,
            coin: "USDC".into(),
            network: "ETH".into(),
            api_amount: "20.5".into(),
            history_amount: "20".into(),
            transaction_fee: "0.5".into(),
            source_free_before: "1000".into(),
            source_free_after: "979.5".into(),
            published_fee: "0.5".into(),
            contract_address: Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into()),
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier {
            asset,
            amount_atoms: Some(fin_domain::Quantity::new("20000000").unwrap()),
            chain_id: 1,
            status: fin_domain::Status::Completed,
            evidence: json!({"fixture":"exact synthetic Ethereum USDC receipt"}),
        })),
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-usdc-withdraw-lifecycle", "account":"main-fixture",
        "destination":"wallet-fixture", "coin":"USDC", "amount":"20.5",
        "network":"ETH", "walletType":"SPOT", "maxFee":"0.5",
        "minReceiveAmount":"20"
    });
    let preview = adapter.prepare("binance.withdraw", &input).await.unwrap();
    let journal = std::sync::Arc::new(RecordingJournal {
        events: events.clone(),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let submission = adapter.submit(&preview, journal).await.unwrap();
    let record = fin_domain::OperationRecord {
        id: "synthetic-usdc-withdraw-op".into(),
        request_id: "synthetic-usdc-withdraw-lifecycle".into(),
        fingerprint: "synthetic-usdc-withdraw-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input,
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(
        result.status,
        fin_domain::Status::Completed,
        "evidence={}",
        result.evidence
    );
    let proof = result.verified.unwrap();
    assert_eq!(proof.effects.len(), 2);
    match &proof.effects[0] {
        fin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.asset.reference, "USDC");
            assert_eq!(debit.atoms.to_decimal(36), "20.5");
            assert_eq!(
                credit.asset.reference,
                "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            );
            assert_eq!(credit.atoms.to_decimal(6), "20");
        }
        other => panic!("unexpected USDC withdrawal effect {other:?}"),
    }
    match &proof.effects[1] {
        fin_domain::FinancialEffect::Cost {
            amount, treatment, ..
        } => {
            assert_eq!(amount.atoms.to_decimal(36), "0.5");
            assert_eq!(treatment, &fin_domain::CostTreatment::IncludedInDebit);
        }
        other => panic!("unexpected USDC withdrawal fee effect {other:?}"),
    }
}

#[tokio::test]
async fn legacy_gross_receipt_floor_is_a_verified_failed_shortfall_not_completion() {
    let result = reconcile_eth_withdrawal_fixture(
        "synthetic-legacy-gross-floor",
        "0.0125",
        "0.01235",
        "0.00015",
        "1.2220",
        "0.00015",
        "0.01235", // valid under the current preview contract
        fin_domain::Status::Completed,
        Some("12350000000000000"),
        Some("0.0125"), // immutable floor from the older approved preview
    )
    .await
    .unwrap();
    assert_eq!(
        result.status,
        fin_domain::Status::Failed,
        "known net shortfall must be terminally recorded as failed: {}",
        result.evidence
    );
    let proof = result
        .verified
        .expect("verified payout and typed partial effects are preserved");
    assert_eq!(proof.effects.len(), 2);
    match &proof.effects[0] {
        fin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.atoms.to_decimal(36), "0.01235");
            assert_eq!(credit.atoms.to_decimal(18), "0.01235");
        }
        other => panic!("expected actual partial payout transfer, got {other:?}"),
    }
    match &proof.effects[1] {
        fin_domain::FinancialEffect::Cost {
            amount, treatment, ..
        } => {
            assert_eq!(amount.atoms.to_decimal(36), "0.00015");
            assert_eq!(*treatment, fin_domain::CostTreatment::AdditionalDebit);
        }
        other => panic!("expected separately charged fee from legacy approval, got {other:?}"),
    }
    assert_eq!(proof.evidence["apiGrossAmount"], "0.0125");
    assert_eq!(proof.evidence["historyNetAmount"], "0.01235");
    assert_eq!(proof.evidence["sourceDebitObserved"], "0.0125");
    assert_eq!(proof.evidence["actual_gross_debit"], "0.0125");
    assert_eq!(proof.evidence["actual_net_receipt"], "0.01235");
    assert_eq!(proof.evidence["actual_fee"], "0.00015");
    assert_eq!(proof.evidence["approved_minimum"], "0.0125");
    assert_eq!(proof.evidence["minimum_receipt_met"], false);
    assert_eq!(proof.evidence["partial_payout_reconciled"], true);
}

#[tokio::test]
async fn withdrawal_reconciliation_rejects_wrong_receipt_overfee_and_gross_debit_mismatch() {
    let wrong_receipt = reconcile_eth_withdrawal_fixture(
        "synthetic-wrong-payout-receipt",
        "0.0125",
        "0.01235",
        "0.00015",
        "1.2220",
        "0.00015",
        "0.01235",
        fin_domain::Status::Failed,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(wrong_receipt.status, fin_domain::Status::Unknown);
    assert!(
        wrong_receipt.verified.is_none(),
        "an unverified/wrong selected-chain receipt cannot produce a terminal proof"
    );

    let overfee = reconcile_eth_withdrawal_fixture(
        "synthetic-overfee",
        "0.0125",
        "0.0123",
        "0.0002",
        "1.2220",
        "0.00015",
        "0.0123",
        fin_domain::Status::Completed,
        Some("12300000000000000"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(overfee.status, fin_domain::Status::Unknown);
    assert!(
        overfee.verified.is_none(),
        "actual fee over its approved cap is unresolved"
    );

    let source_mismatch = reconcile_eth_withdrawal_fixture(
        "synthetic-source-gross-mismatch",
        "0.0125",
        "0.01234",
        "0.00015",
        "1.22201",
        "0.00015",
        "0.01234",
        fin_domain::Status::Completed,
        Some("12340000000000000"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(source_mismatch.status, fin_domain::Status::Unknown);
    assert!(
        source_mismatch.verified.is_none(),
        "authoritative history net plus fee must equal the exact gross API amount"
    );
}

#[tokio::test]
async fn withdrawal_minimum_cannot_exceed_net_after_the_explicit_fee_cap() {
    let error = reconcile_eth_withdrawal_fixture(
        "synthetic-infeasible-receipt-bound",
        "0.0125",
        "0.01235",
        "0.00015",
        "1.2220",
        "0.00015",
        "0.0125",
        fin_domain::Status::Completed,
        Some("12350000000000000"),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "RECEIPT_BOUND_UNSUPPORTED");
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
        dispatch_evidence: None,
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
            coin: "ETH".into(),
            network: "ETH".into(),
            api_amount: "0.0125".into(),
            history_amount: "0.01235".into(),
            transaction_fee: "0.00015".into(),
            source_free_before: "1.2345".into(),
            source_free_after: "1.2220".into(),
            published_fee: "0.00015".into(),
            contract_address: None,
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier {
            asset: fin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            amount_atoms: Some(fin_domain::Quantity::new("12350000000000000").unwrap()),
            chain_id: 1,
            status: fin_domain::Status::Completed,
            evidence: json!({"fixture":"exact synthetic Ethereum receipt"}),
        })),
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-withdraw-timeout", "account":"main-fixture",
        "destination":"wallet-fixture", "coin":"ETH", "amount":"0.0125",
        "network":"ETH", "walletType":"SPOT", "maxFee":"0.00015",
        "minReceiveAmount":"0.01235"
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
        dispatch_evidence: None,
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

struct DiagnosticPrices;
#[async_trait::async_trait]
impl fin_binance::BinancePriceProvider for DiagnosticPrices {
    async fn quote_usd(&self, _coin: &str) -> fin_domain::Result<fin_binance::PriceQuote> {
        Ok(fin_binance::PriceQuote {
            usd_per_unit: "1".into(),
            observed_at: fin_domain::now(),
            source: "synthetic-diagnostic-fixture".into(),
        })
    }
}
