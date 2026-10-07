use fin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint as E,
    BinanceRequest, BinanceTransport, CredentialHandle, TransportError,
};
use fin_domain::{Adapter, ResourceId};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const ADDRESS: &str = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";
struct Fixture {
    response: Value,
    capital_response: Result<Value, TransportError>,
    seen: Mutex<Vec<BinanceRequest>>,
}
#[async_trait::async_trait]
impl BinanceTransport for Fixture {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        assert_eq!(credential.as_str(), "test-only:synthetic-address-book");
        request.validate().unwrap();
        self.seen.lock().unwrap().push(request.clone());
        match request.endpoint {
            E::CapitalConfig => self.capital_response.clone(),
            E::DepositAddressList | E::WithdrawAddressList => Ok(self.response.clone()),
            _ => panic!(
                "Only documented read routes are allowed: {:?}",
                request.endpoint
            ),
        }
    }
}
fn fixture(response: Value, enabled: bool) -> (BinanceAdapter, Arc<Fixture>) {
    fixture_with_capital(
        response,
        Ok(
            json!([{"coin":"ETH","networkList":[{"network":"ETH","coin":"ETH","depositEnable":enabled}]}]),
        ),
    )
}
fn fixture_with_capital(
    response: Value,
    capital_response: Result<Value, TransportError>,
) -> (BinanceAdapter, Arc<Fixture>) {
    let transport = Arc::new(Fixture {
        response,
        capital_response,
        seen: Mutex::new(vec![]),
    });
    let adapter = BinanceAdapter::new(
        BinanceConfig {
            accounts: vec![BinanceAccount {
                name: "synthetic-main".into(),
                resource: ResourceId("binance:synthetic-address-book".into()),
                credential: CredentialHandle::new("test-only:synthetic-address-book").unwrap(),
                role: AccountRole::Main,
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            }],
            destinations: vec![],
        },
        transport.clone(),
        None,
        None,
    );
    (adapter.unwrap(), transport)
}
fn deposit_input() -> Value {
    json!({"account":"synthetic-main","coin":"ETH","network":"ETH"})
}
fn target() -> Value {
    json!({"account":"synthetic-main","coin":"ETH","network":"ETH","address":ADDRESS,"memo":""})
}
fn entry(status: Value) -> Value {
    json!({"coin":"ETH","network":"ETH","address":ADDRESS,"addressTag":"","whiteStatus":status})
}

fn capital_route(coin: &str, network: &str, withdraw_tag: Value) -> Value {
    json!([{"coin":coin,"networkList":[{"coin":coin,"network":network,"withdrawTag":withdraw_tag}]}])
}

#[tokio::test]
async fn whitelist_arbitrum_matches_lowercase_and_mixed_case_hex_addresses() {
    let mixed_case = ADDRESS.replace("abcdef", "aBcDeF");
    let row = json!({"coin":"ETH","network":"ARBITRUM","address":mixed_case,"addressTag":"","whiteStatus":true});
    let (adapter, transport) = fixture(json!([row]), true);
    let mut input = target();
    input["network"] = json!("ARBITRUM");
    let result = adapter.read("binance.whitelist", &input).await.unwrap();
    assert_eq!(result["matchStatus"], "enabled");
    assert_eq!(result["entries"][0]["address"], mixed_case);
    assert_eq!(
        transport.seen.lock().unwrap().len(),
        1,
        "explicit tag needs no metadata inference"
    );
}

#[tokio::test]
async fn whitelist_null_tag_matches_only_after_exact_route_confirms_no_memo() {
    for requested_memo in [Some(""), None] {
        for (white_status, expected) in [(true, "enabled"), (false, "disabled")] {
            let mixed_case = ADDRESS.replace("abcdef", "aBcDeF");
            let (adapter, transport) = fixture_with_capital(
                json!([{"coin":"ETH","network":"ARBITRUM","address":mixed_case,"addressTag":null,"whiteStatus":white_status}]),
                Ok(capital_route("ETH", "ARBITRUM", json!(false))),
            );
            let mut input = target();
            input["network"] = json!("ARBITRUM");
            if requested_memo.is_none() {
                input.as_object_mut().unwrap().remove("memo");
            }
            let result = adapter.read("binance.whitelist", &input).await.unwrap();
            assert_eq!(result["matchStatus"], expected);
            assert_eq!(
                result["entries"][0]["addressTag"],
                Value::Null,
                "inferred matching does not replace missing venue metadata"
            );
            assert_eq!(result["accountWhitelistEnforcement"], "unknown");
            assert_eq!(result["activationStatus"], "unknown");
            let seen = transport.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].endpoint, E::WithdrawAddressList);
            assert_eq!(seen[1].endpoint, E::CapitalConfig);
            assert!(
                seen.iter().all(|request| request.params.is_empty()
                    && request.endpoint.method_path().0 == "GET")
            );
        }
    }
}

#[tokio::test]
async fn whitelist_missing_tag_stays_unknown_for_required_unknown_or_failed_metadata() {
    let input = json!({"account":"synthetic-main","coin":"XRP","network":"XRP","address":"rSyntheticBase58Address","memo":""});
    let row = json!({"coin":"XRP","network":"XRP","address":"rSyntheticBase58Address","addressTag":null,"whiteStatus":true});
    let mut omitted_requirement = capital_route("XRP", "XRP", Value::Null);
    omitted_requirement[0]["networkList"][0]
        .as_object_mut()
        .unwrap()
        .remove("withdrawTag");
    let mut duplicate_route = capital_route("XRP", "XRP", json!(false));
    let route = duplicate_route[0]["networkList"][0].clone();
    duplicate_route[0]["networkList"]
        .as_array_mut()
        .unwrap()
        .push(route);
    for capital_response in [
        Ok(capital_route("XRP", "XRP", json!(true))),
        Ok(capital_route("XRP", "XRP", Value::Null)),
        Ok(omitted_requirement),
        Ok(capital_route("ETH", "XRP", json!(false))),
        Ok(capital_route("XRP", "OTHER", json!(false))),
        Ok(duplicate_route),
        Ok(json!({"malformed":"capital metadata"})),
        Err(TransportError::Unavailable),
    ] {
        let (adapter, transport) = fixture_with_capital(json!([row.clone()]), capital_response);
        assert_eq!(
            adapter.read("binance.whitelist", &input).await.unwrap()["matchStatus"],
            "unknown"
        );
        assert_eq!(transport.seen.lock().unwrap().len(), 2);
    }
    for field in ["coin", "network"] {
        let mut missing = row.clone();
        missing.as_object_mut().unwrap().remove(field);
        let (adapter, transport) = fixture_with_capital(
            json!([missing]),
            Ok(capital_route("XRP", "XRP", json!(false))),
        );
        assert_eq!(
            adapter.read("binance.whitelist", &input).await.unwrap()["matchStatus"],
            "unknown"
        );
        assert_eq!(
            transport.seen.lock().unwrap().len(),
            1,
            "missing row identity cannot be repaired by route metadata"
        );
    }
    let (adapter, transport) = fixture_with_capital(
        json!([row.clone()]),
        Ok(capital_route("XRP", "XRP", json!(false))),
    );
    let mut nonempty_memo = input.clone();
    nonempty_memo["memo"] = json!("00123");
    assert_eq!(
        adapter
            .read("binance.whitelist", &nonempty_memo)
            .await
            .unwrap()["matchStatus"],
        "unknown"
    );
    assert_eq!(
        transport.seen.lock().unwrap().len(),
        1,
        "missing tag cannot match a requested nonempty memo"
    );
    let (adapter, transport) =
        fixture_with_capital(json!([row]), Ok(capital_route("XRP", "XRP", json!(false))));
    assert_eq!(
        adapter
            .read("binance.whitelist", &json!({"account":"synthetic-main"}))
            .await
            .unwrap()["matchStatus"],
        "not_requested"
    );
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn whitelist_preserves_exact_memo_leading_zeros_and_case_sensitive_base58_addresses() {
    let row = json!({"coin":"XRP","network":"XRP","address":"rSyntheticBase58Address","addressTag":"00123","whiteStatus":true});
    let (adapter, transport) = fixture_with_capital(json!([row]), Err(TransportError::Unavailable));
    let mut input = json!({"account":"synthetic-main","coin":"XRP","network":"XRP","address":"rSyntheticBase58Address","memo":"00123"});
    assert_eq!(
        adapter.read("binance.whitelist", &input).await.unwrap()["matchStatus"],
        "enabled"
    );
    input["memo"] = json!("123");
    assert_eq!(
        adapter.read("binance.whitelist", &input).await.unwrap()["matchStatus"],
        "absent"
    );
    input["memo"] = json!("00123");
    input["address"] = json!("rsyntheticBase58Address");
    assert_eq!(
        adapter.read("binance.whitelist", &input).await.unwrap()["matchStatus"],
        "absent"
    );
    assert!(
        transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.endpoint == E::WithdrawAddressList)
    );
}

#[tokio::test]
async fn deposit_list_preserves_default_and_tag_and_never_creates_or_selects_an_address() {
    let (adapter, transport) = fixture(
        json!([{"coin":"ETH","address":ADDRESS,"tag":"exact-tag","isDefault":1,"ignored":"drop"}]),
        true,
    );
    let result = adapter
        .read("binance.deposit-addresses", &deposit_input())
        .await
        .unwrap();
    assert_eq!(result["addresses"][0]["tag"], "exact-tag");
    assert_eq!(result["addresses"][0]["isDefault"], 1);
    assert!(result["addresses"][0].get("ignored").is_none());
    assert_eq!(result["creationSupported"], false);
    {
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1].endpoint.method_path(),
            ("GET", "/sapi/v1/capital/deposit/address/list")
        );
        assert_eq!(seen[1].get("coin"), Some("ETH"));
        assert_eq!(seen[1].get("network"), Some("ETH"));
    }
    let (adapter, _) = fixture(json!([]), true);
    let result = adapter
        .read("binance.deposit-addresses", &deposit_input())
        .await
        .unwrap();
    assert!(result["addresses"].as_array().unwrap().is_empty());
    assert_eq!(result["creationSupported"], false);
    assert!(
        result["humanAction"]
            .as_str()
            .unwrap()
            .contains("Binance Deposit Crypto")
    );
}

#[tokio::test]
async fn malformed_or_disabled_deposit_lists_fail_without_a_write() {
    let valid = json!({"coin":"ETH","address":ADDRESS,"tag":"","isDefault":0});
    let mut malformed = vec![
        json!([valid.clone(), valid.clone()]),
        json!([{"coin":"ETH","address":ADDRESS,"isDefault":1}]),
    ];
    for (field, value) in [
        ("coin", json!("BTC")),
        ("isDefault", json!(2)),
        ("isDefault", json!(true)),
        ("address", json!("not-an-ethereum-address")),
    ] {
        let mut row = valid.clone();
        row[field] = value;
        malformed.push(json!([row]));
    }
    for rows in malformed {
        let (adapter, transport) = fixture(rows, true);
        assert_eq!(
            adapter
                .read("binance.deposit-addresses", &deposit_input())
                .await
                .unwrap_err()
                .code,
            "VENUE_RESPONSE_INVALID"
        );
        assert!(
            transport
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.endpoint.method_path().0 == "GET")
        );
    }
    let (adapter, transport) = fixture(json!([]), false);
    assert_eq!(
        adapter
            .read("binance.deposit-addresses", &deposit_input())
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_DISABLED"
    );
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn whitelist_flags_are_not_global_enforcement_or_activation_proof() {
    for (flag, status) in [
        (json!(true), "enabled"),
        (json!(false), "disabled"),
        (Value::Null, "unknown"),
        (json!("true"), "unknown"),
    ] {
        let (adapter, transport) = fixture(json!([entry(flag)]), true);
        let result = adapter.read("binance.whitelist", &target()).await.unwrap();
        assert_eq!(result["matchStatus"], status);
        assert_eq!(result["accountWhitelistEnforcement"], "unknown");
        assert_eq!(result["activationStatus"], "unknown");
        assert_eq!(result["editingSupported"], false);
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].endpoint, E::WithdrawAddressList);
        assert!(
            seen[0].params.is_empty(),
            "Client-side filters cannot become endpoint parameters"
        );
    }
    let (adapter, _) = fixture(json!([entry(json!(true))]), true);
    let mut capitalized = target();
    capitalized["address"] = json!(format!("0x{}", ADDRESS[2..].to_ascii_uppercase()));
    assert_eq!(
        adapter
            .read("binance.whitelist", &capitalized)
            .await
            .unwrap()["matchStatus"],
        "enabled"
    );
}

#[tokio::test]
async fn whitelist_checks_require_exact_coin_network_memo_and_preserve_missing_identity_as_unknown()
{
    for field in ["coin", "network", "addressTag", "whiteStatus"] {
        let mut row = entry(json!(true));
        row.as_object_mut().unwrap().remove(field);
        let (adapter, _) = fixture(json!([row]), true);
        assert_eq!(
            adapter.read("binance.whitelist", &target()).await.unwrap()["matchStatus"],
            "unknown",
            "{field}"
        );
    }
    // Missing tag cannot establish absence when a specific nonempty memo was requested.
    let mut row = entry(json!(true));
    row.as_object_mut().unwrap().remove("addressTag");
    let (adapter, _) = fixture(json!([row]), true);
    let mut memo = target();
    memo["memo"] = json!("123");
    assert_eq!(
        adapter.read("binance.whitelist", &memo).await.unwrap()["matchStatus"],
        "unknown"
    );
    for (field, value) in [
        ("coin", "BTC"),
        ("network", "BSC"),
        ("addressTag", "different"),
    ] {
        let mut row = entry(json!(true));
        row[field] = json!(value);
        let (adapter, _) = fixture(json!([row]), true);
        assert_eq!(
            adapter.read("binance.whitelist", &target()).await.unwrap()["matchStatus"],
            "absent"
        );
    }
    let (adapter, _) = fixture(json!([entry(json!(true)), entry(json!(false))]), true);
    assert_eq!(
        adapter.read("binance.whitelist", &target()).await.unwrap()["matchStatus"],
        "unknown"
    );
    let (adapter, _) = fixture(json!([]), true);
    assert_eq!(
        adapter.read("binance.whitelist", &target()).await.unwrap()["matchStatus"],
        "absent"
    );
}

#[tokio::test]
async fn unknown_arguments_incomplete_targets_and_oversized_books_cannot_claim_success() {
    for input in [
        json!({"account":"synthetic-main","address":ADDRESS}),
        json!({"account":"synthetic-main","memo":"123"}),
        json!({"account":"synthetic-main","path":"/private/edit"}),
        json!({"account":"synthetic-main","coin":"eth"}),
    ] {
        let (adapter, transport) = fixture(json!([]), true);
        assert!(adapter.read("binance.whitelist", &input).await.is_err());
        assert!(transport.seen.lock().unwrap().is_empty());
    }
    let (adapter, _) = fixture(json!(vec![entry(json!(true)); 1001]), true);
    assert_eq!(
        adapter
            .read("binance.whitelist", &target())
            .await
            .unwrap_err()
            .code,
        "ADDRESS_BOOK_TOO_LARGE"
    );
    let (adapter, _) = fixture(
        json!(vec![
            json!({"coin":"ETH","address":ADDRESS,"tag":"","isDefault":1});
            1001
        ]),
        true,
    );
    assert_eq!(
        adapter
            .read("binance.deposit-addresses", &deposit_input())
            .await
            .unwrap_err()
            .code,
        "ADDRESS_BOOK_TOO_LARGE"
    );
}
