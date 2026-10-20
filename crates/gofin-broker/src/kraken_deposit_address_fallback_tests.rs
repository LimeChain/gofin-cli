use super::*;

fn unavailable_address_read(status: u16) -> GoFinError {
    error("KRAKEN_UNAVAILABLE", "Synthetic address read failure.").with_details(json!({
        "operation":E::KrakenFundingGetFundingV2DepositAddresses.operation(),
        "transport_kind":"Rejected", "http_status":status,
    }))
}
fn selected_input() -> Value {
    let mut value = input();
    value["address"] = json!("0x1111111111111111111111111111111111111111");
    value
}
#[tokio::test]
async fn server_error_uses_only_exact_existing_address_and_preserves_financial_scope() {
    let (adapter, venue) = adapter();
    let original = adapter
        .prepare("kraken.deposit", &selected_input())
        .await
        .unwrap();
    *venue.address_read_failure.lock().unwrap() = Some(unavailable_address_read(500));
    let fallback = adapter
        .prepare("kraken.deposit", &selected_input())
        .await
        .unwrap();
    assert_eq!(fallback.intent, original.intent);
    assert_eq!(fallback.binding["method_id"], original.binding["method_id"]);
    assert_eq!(fallback.binding["address"], original.binding["address"]);
    assert_eq!(
        fallback.binding["source_asset"],
        original.binding["source_asset"]
    );
    assert_eq!(
        fallback.binding["route"]["fee"],
        original.binding["route"]["fee"]
    );
    assert_eq!(
        fallback.binding["route"]["limits"],
        original.binding["route"]["limits"]
    );
    assert_eq!(
        fallback.binding["discovery_api"],
        "funding-v1-legacy-address-read"
    );
    let requests = venue.requests.lock().unwrap();
    let legacy = requests
        .iter()
        .find(|(e, _)| *e == E::KrakenPrivatePostDepositAddresses)
        .unwrap();
    assert_eq!(
        legacy.1,
        json!({"asset":"ETH","aclass":"currency","method":"Ether (Hex) (Unified)"})
    );
    assert!(requests.iter().all(|(e, _)| !e.mutating()));
    assert!(
        !venue
            .rpc
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "eth_sendRawTransaction")
    );
}
#[tokio::test]
async fn live_usdc_unified_suffix_requires_unique_advertised_method_and_exact_address() {
    let (adapter, venue) = adapter();
    *venue.legacy_methods.lock().unwrap() = json!([{"method":"USDC - Ethereum (Unified)"}]);
    let method = json!({"method_id":"current-method","method_name":"USDC - Ethereum","asset":{"class":"currency","name":"USDC"},"network":{"network_name":"Ethereum","contract_address":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}});
    let input = KrakenEvmDepositAdapter::input(&selected_input()).unwrap();
    let rows = adapter
        .legacy_address_witness(&input, &method, std::slice::from_ref(&method), "family")
        .await
        .unwrap();
    assert_eq!(
        rows[0]["verification"]["method"],
        "USDC - Ethereum (Unified)"
    );
    assert_eq!(
        rows[0]["address_details"]["crypto"]["address"],
        input.address.clone().unwrap()
    );
    for methods in [
        json!([{"method":"USDC - Ethereum"},{"method":"USDC - Ethereum (Unified)"}]),
        json!([{"method":"USDC - Base (Unified)"}]),
        json!([{"method":"USDC - Ethereum (Other)"}]),
    ] {
        *venue.legacy_methods.lock().unwrap() = methods;
        assert_eq!(
            adapter
                .legacy_address_witness(&input, &method, std::slice::from_ref(&method), "family")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_ROUTE_AMBIGUOUS"
        );
    }
}
#[tokio::test]
async fn fallback_never_overrides_authentication_successful_mismatch_or_missing_selection() {
    for status in [400, 401, 403, 429] {
        let (adapter, venue) = adapter();
        *venue.address_read_failure.lock().unwrap() = Some(unavailable_address_read(status));
        let failure = adapter
            .prepare("kraken.deposit", &selected_input())
            .await
            .unwrap_err();
        assert_eq!(failure.details.as_deref().unwrap()["http_status"], status);
        assert!(
            !venue
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _)| *e == E::KrakenPrivatePostDepositMethods)
        );
    }
    for (code, kind, native) in [
        (
            "KRAKEN_AUTHENTICATION_FAILED",
            "Authentication",
            Value::Null,
        ),
        ("KRAKEN_INVALID_RESPONSE", "InvalidResponse", Value::Null),
        ("KRAKEN_UNAVAILABLE", "Rejected", json!("unauthorized")),
        ("KRAKEN_UNAVAILABLE", "Rejected", json!("invalid_argument")),
    ] {
        let (adapter, venue) = adapter();
        *venue.address_read_failure.lock().unwrap() = Some(error(code, "Synthetic failure").with_details(json!({
            "operation":E::KrakenFundingGetFundingV2DepositAddresses.operation(), "http_status":500,
            "transport_kind":kind, "funding_error_code":native,
        })));
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &selected_input())
                .await
                .unwrap_err()
                .code,
            code
        );
        assert!(
            !venue
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _)| *e == E::KrakenPrivatePostDepositMethods)
        );
    }
    let (adapter, venue) = adapter();
    let mut mismatch = selected_input();
    mismatch["address"] = json!("0x9999999999999999999999999999999999999999");
    assert_eq!(
        adapter
            .prepare("kraken.deposit", &mismatch)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_DEPOSIT_ADDRESS_REQUIRED"
    );
    assert!(
        !venue
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(e, _)| *e == E::KrakenPrivatePostDepositMethods)
    );
    *venue.address_read_failure.lock().unwrap() = Some(unavailable_address_read(500));
    assert_eq!(
        adapter
            .prepare("kraken.deposit", &input())
            .await
            .unwrap_err()
            .code,
        "KRAKEN_DEPOSIT_ADDRESS_REQUIRED"
    );
}
#[tokio::test]
async fn fallback_rejects_expired_duplicate_memo_or_wrong_address_before_wallet_preparation() {
    for rows in [
        json!([{"address":"0x1111111111111111111111111111111111111111","expiretm":"1"}]),
        json!([{"address":"0x1111111111111111111111111111111111111111","expiretm":"invalid"}]),
        json!([{"address":"0x1111111111111111111111111111111111111111","expiretm":"0","memo":"required"}]),
        json!([{"address":"0x1111111111111111111111111111111111111111","expiretm":"0"},{"address":"0x1111111111111111111111111111111111111111","expiretm":"0"}]),
        json!([{"address":"0x9999999999999999999999999999999999999999","expiretm":"0"}]),
        json!({"malformed":true}),
    ] {
        let (adapter, venue) = adapter();
        *venue.address_read_failure.lock().unwrap() = Some(unavailable_address_read(500));
        *venue.legacy_addresses.lock().unwrap() = rows;
        assert!(
            adapter
                .prepare("kraken.deposit", &selected_input())
                .await
                .is_err()
        );
        assert!(venue.rpc.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn current_usdc_query_keeps_selected_method_despite_shared_address_family() {
    let (adapter, venue) = adapter();
    venue.usdc_evm.store(true, Ordering::SeqCst);
    venue.rpc.usdc_evm.store(true, Ordering::SeqCst);
    let mut input = selected_input();
    input["amount"] = json!("100");
    input["token"] = json!(usdc_fixture::TOKEN);
    let prepared = adapter.prepare("kraken.deposit", &input).await.unwrap();
    assert_eq!(prepared.binding["method_id"], input["methodId"]);
    assert_eq!(
        prepared.binding["route"]["address"]["method_id"],
        input["methodId"]
    );
    assert_eq!(prepared.binding["address"], input["address"]);
    assert_eq!(
        prepared.binding["source_asset"]["reference"],
        usdc_fixture::TOKEN
    );
    assert_eq!(prepared.binding["discovery_api"], "funding-v1-v2");
    let requests = venue.requests.lock().unwrap();
    let query = requests
        .iter()
        .find(|(e, _)| *e == E::KrakenFundingGetFundingV2DepositAddresses)
        .unwrap();
    assert_eq!(query.1["scope"]["method_id"], input["methodId"]);
    assert_ne!(
        query.1["scope"]["method_id"],
        prepared.binding["route"]["method"]["deposit"]["shares_addresses_with_method_id"]
    );
    assert!(
        !requests
            .iter()
            .any(|(e, _)| *e == E::KrakenPrivatePostDepositAddresses)
    );
}

#[tokio::test]
async fn claimed_address_continuation_uses_only_cursor_and_preserves_selected_destination() {
    struct Pages {
        inner: Arc<Venue>,
        queries: Mutex<Vec<Value>>,
    }
    #[async_trait]
    impl KrakenTransport for Pages {
        async fn request(
            &self,
            endpoint: E,
            account: Option<&KrakenAccount>,
            parameters: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            if endpoint != E::KrakenFundingGetFundingV2DepositAddresses {
                return self
                    .inner
                    .request(endpoint, account, parameters, guard)
                    .await;
            }
            assert!(guard.is_none());
            self.queries.lock().unwrap().push(parameters.clone());
            Ok(KrakenResponse::Json(
                if parameters.get("cursor").is_none() {
                    assert_eq!(
                        parameters,
                        json!({"scope":{"method_id":"00000000-0000-0000-0000-000000000001"},"limit":500})
                    );
                    json!({"addresses":[],"next_cursor":"fixture-continuation"})
                } else {
                    assert_eq!(parameters, json!({"cursor":"fixture-continuation"}));
                    json!({"addresses":[{"method_id":"00000000-0000-0000-0000-000000000001","address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}}]})
                },
            ))
        }
    }
    let (mut adapter, venue) = adapter();
    let pages = Arc::new(Pages {
        inner: venue,
        queries: Mutex::new(vec![]),
    });
    adapter.kraken = Arc::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "fixture-spot".into(),
                resource: ResourceId("kraken:fixture-spot".into()),
                credential_ref: "synthetic-credential-only".into(),
                physical_identity: "synthetic-uid-only".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            pages.clone(),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    );
    let prepared = adapter
        .prepare("kraken.deposit", &selected_input())
        .await
        .unwrap();
    assert_eq!(prepared.binding["address"], selected_input()["address"]);
    assert_eq!(pages.queries.lock().unwrap().len(), 2);
}
