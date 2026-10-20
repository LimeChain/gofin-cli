//! Coinbase Advanced USDC on Base withdrawal contract and adapter behavior.
use async_trait::async_trait;
use gofin_coinbase::contracts::{self, Action, Product};
use gofin_coinbase::funding::{
    CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult, CoinbaseRecipientVerifier,
};
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::{AssetId, CostTreatment, ResourceId};
use gofin_exchange::ExchangeProfile;
use gofin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, CoinbaseRoute,
    DispatchGuard,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const PORTFOLIO: &str = "00000000-0000-4000-8000-000000000001";
const ACCOUNT_ID: &str = "00000000-0000-4000-8000-000000000002";
const WALLET: &str = "0x1111111111111111111111111111111111111111";
const BASE_USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

struct ReadApi {
    available_usdc: Arc<std::sync::Mutex<String>>,
}
#[async_trait]
impl CoinbaseApi for ReadApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none(), "preparation must not dispatch a write");
        let body = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":PORTFOLIO,"can_view":true,"can_transfer":true})
            }
            CoinbaseRoute::LegacyAccounts { .. } => {
                json!({"data":[{"id":ACCOUNT_ID,"currency":{"code":"USDC"}}],"pagination":{"next_starting_after":null}})
            }
            CoinbaseRoute::Accounts { .. } => {
                json!({"accounts":[{"uuid":ACCOUNT_ID,"currency":"USDC","portfolio_id":PORTFOLIO,"active":true,"ready":true}],"has_next":false})
            }
            CoinbaseRoute::Account { account_id } => {
                assert_eq!(account_id, ACCOUNT_ID);
                json!({"account":{"uuid":ACCOUNT_ID,"currency":"USDC","active":true,"ready":true,"available_balance":{"value":self.available_usdc.lock().unwrap().clone(),"currency":"USDC"}}})
            }
            other => panic!("unexpected read endpoint {other:?}"),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}

struct Recipient {
    resolved: CoinbaseDestination,
}
#[async_trait]
impl CoinbaseRecipientVerifier for Recipient {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> gofin_domain::Result<Option<CoinbaseDestination>> {
        assert_eq!(
            (network, coin, contract, destination_name),
            (
                network,
                "USDC",
                Some(
                    gofin_coinbase::funding::usdc_withdrawal_network(network)
                        .unwrap()
                        .contract
                ),
                Some("evm-dev")
            )
        );
        assert!(!address.is_empty());
        Ok(Some(self.resolved.clone()))
    }
    async fn verify(&self, _: CoinbasePayoutRequest) -> gofin_domain::Result<CoinbasePayoutResult> {
        unreachable!("preparation never uses settlement proof")
    }
}

fn recipient() -> CoinbaseDestination {
    CoinbaseDestination {
        enrolled: true,
        resource: ResourceId("ethereum:8453:evm-dev".into()),
        address: WALLET.into(),
        chain_id: 8453,
        asset: AssetId {
            network: "ethereum:8453".into(),
            reference: BASE_USDC.into(),
            decimals: 6,
        },
        confirmations: 1,
        network: "base".into(),
    }
}

fn profile(destination: CoinbaseDestination) -> CoinbaseProfile {
    profile_with_balance(destination, "5.000000").0
}

fn profile_with_balance(
    destination: CoinbaseDestination,
    balance: &str,
) -> (CoinbaseProfile, Arc<std::sync::Mutex<String>>) {
    let available_usdc = Arc::new(std::sync::Mutex::new(balance.to_owned()));
    let api: Arc<dyn CoinbaseApi> = Arc::new(ReadApi {
        available_usdc: available_usdc.clone(),
    });
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "coinbase-dev".into(),
            resource: ResourceId(format!("coinbase:portfolio:{PORTFOLIO}")),
            credential_ref: "synthetic".into(),
            physical_identity: PORTFOLIO.into(),
            identity_verified: true,
            portfolio_id: Some(PORTFOLIO.into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("synthetic".into(), api)]),
    )
    .unwrap()
    .with_prices(Arc::new(TestPrices))
    .with_recipient_verifier(Arc::new(Recipient {
        resolved: destination,
    }));
    (profile, available_usdc)
}

struct TestPrices;
#[async_trait]
impl UsdPriceSource for TestPrices {
    async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: gofin_domain::now(),
            observed_at: gofin_domain::now(),
            source: "synthetic USDC price".into(),
        })
    }
}

fn withdraw_input() -> Value {
    json!({
        "account":"coinbase-dev","accountId":ACCOUNT_ID,"destination":"evm-dev",
        "address":WALLET,"network":"base","amount":"4.000000",
        "feeMode":"venue_calculated","feeReserveUsdc":"0.500000","requestId":"withdrawal-test"
    })
}

#[test]
fn withdrawal_contract_requires_an_explicit_venue_calculated_fee_reserve() {
    let contract = contracts::contract("coinbase.withdraw")
        .expect("Advanced withdrawal is a discoverable Coinbase operation");
    assert_eq!(contract.product, Product::Advanced);
    assert_eq!(contract.action, Action::Write);
    let schema = (contract.schema)();
    let required = schema["required"].as_array().unwrap();
    for key in [
        "account",
        "accountId",
        "destination",
        "address",
        "network",
        "amount",
        "feeMode",
        "feeReserveUsdc",
        "requestId",
    ] {
        assert!(required.iter().any(|value| value == key), "missing {key}");
    }
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["feeMode"]["const"], "venue_calculated");
    assert_eq!(
        schema["properties"]["network"]["enum"],
        json!([
            "ethereum",
            "base",
            "arbitrum",
            "optimism",
            "polygon",
            "avalanche"
        ])
    );
    assert_eq!(
        schema["properties"]["feeReserveUsdc"]["allOf"][0]["type"],
        "string"
    );
    let description = contract.description.to_ascii_lowercase();
    assert!(description.contains("fee") && description.contains("reserve"));
}

#[test]
fn app_transaction_send_uses_the_exact_scoped_account_and_a_mutating_route() {
    let route = CoinbaseRoute::TransactionSend {
        account_id: "usdc/account".into(),
    };
    assert_eq!(route.method(), "POST");
    assert_eq!(
        route.path().unwrap(),
        "/v2/accounts/usdc%2Faccount/transactions"
    );
    assert!(route.mutating());
}

#[test]
fn withdrawal_schema_rejects_ambiguous_fee_cap_or_network_inputs() {
    let contract = contracts::contract("coinbase.withdraw").unwrap();
    let schema = (contract.schema)();
    let properties = schema["properties"].as_object().unwrap();
    assert!(!properties.contains_key("maxFeeUsdc"));
    assert_eq!(
        schema["properties"]["feeMode"]["enum"],
        json!(["venue_calculated"])
    );
    assert_eq!(
        schema["properties"]["network"]["enum"],
        json!([
            "ethereum",
            "base",
            "arbitrum",
            "optimism",
            "polygon",
            "avalanche"
        ])
    );
}

fn validates_schema_instance(schema: &Value, input: &Value) -> bool {
    // Keep the contract tests independent of a particular JSON Schema crate:
    // assert the closed-object and fixed-enum rules used by this small schema.
    if !input.is_object() || schema["additionalProperties"] != false {
        return false;
    }
    let props = schema["properties"].as_object().unwrap();
    let input = input.as_object().unwrap();
    if input.keys().any(|key| !props.contains_key(key)) {
        return false;
    }
    if schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|key| input.get(key.as_str().unwrap()).is_none())
    {
        return false;
    }
    ["feeMode", "network"].into_iter().all(|key| {
        props[key]["const"].as_str().map_or_else(
            || {
                props[key]["enum"]
                    .as_array()
                    .is_some_and(|values| values.contains(&input[key]))
            },
            |fixed| input[key] == fixed,
        )
    })
}

#[test]
fn explicit_ack_and_finite_reserve_are_required_by_public_contract() {
    let schema = (contracts::contract("coinbase.withdraw").unwrap().schema)();
    let request = json!({
        "account":"coinbase-dev",
        "accountId":"usdc-account-1",
        "destination":"evm-dev",
        "address":"0x1111111111111111111111111111111111111111",
        "network":"base",
        "amount":"5.000000",
        "feeMode":"venue_calculated",
        "feeReserveUsdc":"0.500000",
        "requestId":"test-withdraw-1"
    });
    assert!(validates_schema_instance(&schema, &request));
    for changed in [
        json!({"feeMode":""}),
        json!({"feeMode":"venue_calculated","network":"unsupported"}),
        json!({"feeMode":"venue_calculated","network":"base","maxFeeUsdc":"0.01"}),
    ] {
        let mut invalid = request.clone();
        for (key, value) in changed.as_object().unwrap() {
            invalid[key] = value.clone();
        }
        assert!(!validates_schema_instance(&schema, &invalid));
    }
    let mut missing = request;
    missing.as_object_mut().unwrap().remove("feeMode");
    assert!(!validates_schema_instance(&schema, &missing));
}

#[tokio::test]
async fn prepare_binds_exact_usdc_base_destination_and_accounts_for_reserved_fee() {
    let profile = profile(recipient());
    let prepared = profile
        .prepare("coinbase.withdraw", &withdraw_input())
        .await
        .unwrap();
    assert_eq!(prepared.binding["currencyAccountId"], ACCOUNT_ID);
    assert_eq!(prepared.binding["network"], "base");
    assert_eq!(prepared.binding["destinationAddress"], WALLET);
    assert_eq!(prepared.binding["feeMode"], "venue_calculated");
    assert_eq!(prepared.binding["feeReserveUsdc"], "0.500000");
    let request: Value =
        serde_json::from_str(prepared.binding["requestBodyExact"].as_str().unwrap()).unwrap();
    assert_eq!(request["currency"], "USDC");
    assert_eq!(request["network"], "base");
    assert_eq!(request["to"], WALLET);
    let debit = &prepared.intent.debits[0].amount.atoms;
    assert_eq!(debit.to_decimal(36), "4");
    assert_eq!(prepared.intent.maximum_costs.len(), 1);
    assert_eq!(
        prepared.intent.maximum_costs[0].amount.atoms.to_decimal(36),
        "0.5"
    );
    assert_eq!(
        prepared.intent.maximum_costs[0].treatment,
        CostTreatment::VenueCalculatedAdditionalDebit
    );
}

#[tokio::test]
async fn planned_send_reuses_the_bound_uuid_and_exact_app_account() {
    let profile = profile(recipient());
    let prepared = profile
        .prepare("coinbase.withdraw", &withdraw_input())
        .await
        .unwrap();
    let idem = prepared.binding["idem"].as_str().unwrap().to_owned();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    assert_eq!(planned.submission.reference, idem);
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::TransactionSend {
            account_id: ACCOUNT_ID.into()
        }
    );
    let body: Value = serde_json::from_str(&planned.request.exact_body).unwrap();
    assert_eq!(body["idem"], idem);
    assert_eq!(body["type"], "send");
    assert_eq!(body["to"], WALLET);
    assert_eq!(body["network"], "base");
    assert_eq!(body["currency"], "USDC");
    assert_eq!(body["amount"], "4.000000");
}

fn travel_rule_data() -> Value {
    json!({
        "beneficiary_wallet_type":"WALLET_TYPE_SELF_HOSTED",
        "is_self":"IS_SELF_TRUE",
        "beneficiary_name":"Synthetic Test Beneficiary",
        "beneficiary_address":{"country":"US"}
    })
}

#[tokio::test]
async fn travel_rule_is_closed_validated_bound_and_preserved_through_refresh() {
    let configured = profile(recipient());
    let mut input = withdraw_input();
    input["travelRuleData"] = travel_rule_data();
    let prepared = configured
        .prepare("coinbase.withdraw", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["travelRuleData"], input["travelRuleData"]);

    let native: Value =
        serde_json::from_str(prepared.binding["requestBodyExact"].as_str().unwrap()).unwrap();
    assert_eq!(native["travel_rule_data"], input["travelRuleData"]);
    assert_eq!(native["idem"], prepared.binding["idem"]);

    let refreshed = configured.refresh_prepared(&prepared).await.unwrap();
    assert_eq!(refreshed.binding["travelRuleData"], input["travelRuleData"]);
    assert_eq!(
        refreshed.binding["requestBodyExact"],
        prepared.binding["requestBodyExact"]
    );
    let planned = configured.plan_submission(&refreshed).await.unwrap();
    let planned_body: Value = serde_json::from_str(&planned.request.exact_body).unwrap();
    assert_eq!(planned_body["travel_rule_data"], input["travelRuleData"]);
}

#[tokio::test]
async fn travel_rule_rejects_unknown_fields_enums_false_self_bad_country_and_control_names() {
    let configured = profile(recipient());
    let mut cases = Vec::new();

    let mut unknown = travel_rule_data();
    unknown["extra"] = json!("must be rejected");
    cases.push(unknown);

    let mut wrong_wallet_type = travel_rule_data();
    wrong_wallet_type["beneficiary_wallet_type"] = json!("WALLET_TYPE_EXCHANGE");
    cases.push(wrong_wallet_type);

    let mut false_self = travel_rule_data();
    false_self["is_self"] = json!("IS_SELF_FALSE");
    cases.push(false_self);

    for country in ["USA", "us", "U1"] {
        let mut bad_country = travel_rule_data();
        bad_country["beneficiary_address"]["country"] = json!(country);
        cases.push(bad_country);
    }

    for name in ["", "Synthetic\nBeneficiary", "Synthetic\u{0007}Beneficiary"] {
        let mut bad_name = travel_rule_data();
        bad_name["beneficiary_name"] = json!(name);
        cases.push(bad_name);
    }

    let mut missing_member = travel_rule_data();
    missing_member.as_object_mut().unwrap().remove("is_self");
    cases.push(missing_member);

    for (index, travel) in cases.into_iter().enumerate() {
        let mut request = withdraw_input();
        request["travelRuleData"] = travel;
        assert!(
            configured
                .prepare("coinbase.withdraw", &request)
                .await
                .is_err(),
            "invalid Travel Rule case {index} was accepted"
        );
    }
}

#[tokio::test]
async fn travel_rule_schema_and_runtime_agree_on_unicode_name_length_and_controls() {
    let schema = (contracts::contract("coinbase.withdraw").unwrap().schema)();
    let validator = jsonschema::options().offline().build(&schema).unwrap();
    let configured = profile(recipient());
    let mut request = withdraw_input();
    request["travelRuleData"] = travel_rule_data();
    request["travelRuleData"]["beneficiary_name"] = json!("界".repeat(150));
    assert!(validator.is_valid(&request));
    configured
        .prepare("coinbase.withdraw", &request)
        .await
        .unwrap();
    for name in [
        "界".repeat(201),
        "   ".into(),
        "Synthetic\u{0085}Name".into(),
        "Synthetic\nName".into(),
    ] {
        request["travelRuleData"]["beneficiary_name"] = json!(name);
        assert!(!validator.is_valid(&request));
        assert!(
            configured
                .prepare("coinbase.withdraw", &request)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn preparation_rejects_omitted_fee_ack_and_destination_account_network_mismatches() {
    let configured = profile(recipient());
    let mut input = withdraw_input();
    input.as_object_mut().unwrap().remove("feeMode");
    assert!(
        configured
            .prepare("coinbase.withdraw", &input)
            .await
            .is_err()
    );

    let mut input = withdraw_input();
    input["accountId"] = json!("00000000-0000-4000-8000-000000000099");
    let error = configured
        .prepare("coinbase.withdraw", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_APP_ACCOUNT_IDENTITY_MISMATCH");

    let mut input = withdraw_input();
    input["network"] = json!("ethereum");
    assert!(
        configured
            .prepare("coinbase.withdraw", &input)
            .await
            .is_err()
    );

    let mut wrong = recipient();
    wrong.chain_id = 1;
    assert_eq!(
        profile(wrong)
            .prepare("coinbase.withdraw", &withdraw_input())
            .await
            .unwrap_err()
            .code,
        "COINBASE_DESTINATION_IDENTITY_MISMATCH"
    );

    let mut input = withdraw_input();
    input["address"] = json!("0x2222222222222222222222222222222222222222");
    assert_eq!(
        profile(recipient())
            .prepare("coinbase.withdraw", &input)
            .await
            .unwrap_err()
            .code,
        "COINBASE_DESTINATION_IDENTITY_MISMATCH"
    );
}

#[tokio::test]
async fn preparation_reserves_finite_fee_budget_in_available_source_balance() {
    let profile = profile(recipient());
    let mut input = withdraw_input();
    input["amount"] = json!("4.600000");
    input["feeReserveUsdc"] = json!("0.500000");
    let error = profile
        .prepare("coinbase.withdraw", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE");
}

#[tokio::test]
async fn fractional_source_balance_is_compared_at_36_decimals_through_refresh_and_planning() {
    let (profile, balance) = profile_with_balance(recipient(), "4.5000001");
    let prepared = profile
        .prepare("coinbase.withdraw", &withdraw_input())
        .await
        .expect("sub-micro source balance above principal plus reserve is sufficient");
    assert_eq!(prepared.binding["sourceAvailableBefore"], "4.5000001");

    let refreshed = profile.refresh_prepared(&prepared).await.unwrap();
    assert_eq!(refreshed.binding["sourceAvailableBefore"], "4.5000001");
    profile
        .plan_submission(&refreshed)
        .await
        .expect("pre-dispatch balance revalidation must retain source precision");

    // A seven-decimal balance below the combined requirement must still fail.
    *balance.lock().unwrap() = "4.4999999".into();
    let error = profile
        .prepare("coinbase.withdraw", &withdraw_input())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE");
}

#[tokio::test]
async fn outgoing_base_principal_remains_limited_to_six_decimals() {
    let profile = profile(recipient());
    let mut input = withdraw_input();
    input["amount"] = json!("4.0000001");
    assert!(profile.prepare("coinbase.withdraw", &input).await.is_err());
}

#[tokio::test]
async fn fractional_balance_drop_after_prepare_is_rejected_before_dispatch() {
    let (profile, balance) = profile_with_balance(recipient(), "4.5000001");
    let prepared = profile
        .prepare("coinbase.withdraw", &withdraw_input())
        .await
        .unwrap();
    *balance.lock().unwrap() = "4.4999999".into();
    let refresh_error = profile.refresh_prepared(&prepared).await.unwrap_err();
    assert_eq!(
        refresh_error.code,
        "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE"
    );
    let plan_error = profile
        .plan_submission(&prepared)
        .await
        .err()
        .expect("fresh pre-dispatch balance check must reject the insufficient reserve");
    assert_eq!(plan_error.code, "COINBASE_WITHDRAWAL_BALANCE_CHANGED");
}

#[tokio::test]
async fn exact_withdrawal_plan_feasibility_defers_funds_but_preserves_live_checks() {
    let (profile, _) = profile_with_balance(recipient(), "0");
    let warnings = profile
        .validate_plan_feasibility("coinbase.withdraw", &withdraw_input(), None)
        .await
        .unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("principal-only"))
    );
    assert_eq!(
        profile
            .prepare("coinbase.withdraw", &withdraw_input())
            .await
            .unwrap_err()
            .code,
        "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE"
    );
    let mut invalid = withdraw_input();
    invalid["amount"] = json!("1.0000001");
    assert!(
        profile
            .validate_plan_feasibility("coinbase.withdraw", &invalid, None)
            .await
            .is_err()
    );
    invalid = withdraw_input();
    invalid["address"] = json!("0x2222222222222222222222222222222222222222");
    assert!(
        profile
            .validate_plan_feasibility("coinbase.withdraw", &invalid, None)
            .await
            .is_err()
    );
}

fn destination_for(route: &gofin_coinbase::funding::CoinbaseUsdcNetwork) -> CoinbaseDestination {
    let mut destination = recipient();
    destination.network = route.network.into();
    destination.chain_id = route.chain_id;
    destination.resource = ResourceId(format!("ethereum:{}:evm-dev", route.chain_id));
    destination.asset.network = format!("ethereum:{}", route.chain_id);
    destination.asset.reference = route.contract.into();
    destination
}

#[tokio::test]
async fn every_supported_network_binds_exact_native_usdc_and_defers_only_future_funds() {
    for route in gofin_coinbase::funding::USDC_WITHDRAWAL_NETWORKS {
        let destination = destination_for(route);
        let mut input = withdraw_input();
        input["network"] = json!(route.network);
        let (profile, balance) = profile_with_balance(destination.clone(), "0");
        profile
            .validate_plan_feasibility("coinbase.withdraw", &input, None)
            .await
            .unwrap();
        assert_eq!(
            profile
                .prepare("coinbase.withdraw", &input)
                .await
                .unwrap_err()
                .code,
            "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE"
        );
        *balance.lock().unwrap() = "5".into();
        let prepared = profile.prepare("coinbase.withdraw", &input).await.unwrap();
        assert_eq!(prepared.binding["version"], 1);
        assert_eq!(prepared.binding["network"], route.network);
        assert_eq!(
            prepared.binding["destination"],
            serde_json::to_value(&destination).unwrap()
        );
        let planned = profile.plan_submission(&prepared).await.unwrap();
        let body: Value = serde_json::from_str(&planned.request.exact_body).unwrap();
        assert_eq!(body["network"], route.network);
        assert_eq!(body["amount"], input["amount"]);
        assert_eq!(body["idem"], planned.submission.reference);
        let mut changed = prepared.clone();
        changed.binding["network"] = json!(if route.network == "base" {
            "ethereum"
        } else {
            "base"
        });
        assert_eq!(
            profile.plan_submission(&changed).await.err().unwrap().code,
            "COINBASE_DESTINATION_IDENTITY_MISMATCH"
        );
    }
}

#[tokio::test]
async fn all_routes_reject_wrong_contract_without_dispatch() {
    for route in gofin_coinbase::funding::USDC_WITHDRAWAL_NETWORKS {
        let mut input = withdraw_input();
        input["network"] = json!(route.network);
        let mut destination = destination_for(route);
        destination.asset.reference = "0x0000000000000000000000000000000000000001".into();
        assert_eq!(
            profile(destination)
                .prepare("coinbase.withdraw", &input)
                .await
                .unwrap_err()
                .code,
            "COINBASE_DESTINATION_IDENTITY_MISMATCH"
        );
    }
}

struct RecoveryApi {
    reads: ReadApi,
    idem: std::sync::Mutex<String>,
    network: std::sync::Mutex<String>,
}
#[async_trait]
impl CoinbaseApi for RecoveryApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none(), "recovery must never dispatch");
        let body = match endpoint {
            CoinbaseRoute::Transactions { account_id, .. } => {
                assert_eq!(account_id, ACCOUNT_ID);
                json!({"data":[{"id":"original-send","idem":self.idem.lock().unwrap().clone()}],"pagination":{"next_starting_after":null}})
            }
            CoinbaseRoute::Transaction {
                account_id,
                transaction_id,
            } => {
                assert_eq!(account_id, ACCOUNT_ID);
                assert_eq!(transaction_id, "original-send");
                json!({"data":{"id":"original-send","idem":self.idem.lock().unwrap().clone(),"type":"send","status":"completed","amount":{"amount":"-4.1","currency":"USDC"},"to":{"address":WALLET},"network":{"network_name":self.network.lock().unwrap().clone(),"hash":format!("0x{}", "ab".repeat(32)),"transaction_amount":{"amount":"4","currency":"USDC"},"transaction_fee":{"amount":"0.1","currency":"USDC"}}}})
            }
            other => return self.reads.request(other, request, None).await,
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}
struct SettlementRecipient(CoinbaseDestination);
#[async_trait]
impl CoinbaseRecipientVerifier for SettlementRecipient {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> gofin_domain::Result<Option<CoinbaseDestination>> {
        assert_eq!(network, self.0.network);
        assert_eq!(coin, "USDC");
        assert_eq!(contract, Some(self.0.asset.reference.as_str()));
        assert_eq!(address, self.0.address);
        assert_eq!(destination_name, Some("evm-dev"));
        Ok(Some(self.0.clone()))
    }
    async fn verify(
        &self,
        request: CoinbasePayoutRequest,
    ) -> gofin_domain::Result<CoinbasePayoutResult> {
        assert_eq!(
            serde_json::to_value(request.destination).unwrap(),
            serde_json::to_value(&self.0).unwrap()
        );
        assert_eq!(
            (request.minimum.as_str(), request.maximum.as_str()),
            ("4.000000", "4.000000")
        );
        assert_eq!(request.tx_hash, format!("0x{}", "ab".repeat(32)));
        Ok(CoinbasePayoutResult {
            status: gofin_domain::Status::Completed,
            amount: Some(gofin_domain::Quantity::new("4000000").unwrap()),
            evidence: json!({"exactChainTokenAndTransfer":true}),
        })
    }
}

#[tokio::test]
async fn all_networks_recover_original_unknown_submission_and_reject_cross_network_receipts() {
    for route in gofin_coinbase::funding::USDC_WITHDRAWAL_NETWORKS {
        let api = Arc::new(RecoveryApi {
            reads: ReadApi {
                available_usdc: Arc::new(std::sync::Mutex::new("5".into())),
            },
            idem: std::sync::Mutex::new(String::new()),
            network: std::sync::Mutex::new(route.network.into()),
        });
        let profile = CoinbaseProfile::new(
            vec![CoinbaseAccount {
                name: "coinbase-dev".into(),
                resource: ResourceId(format!("coinbase:portfolio:{PORTFOLIO}")),
                credential_ref: "synthetic".into(),
                physical_identity: PORTFOLIO.into(),
                identity_verified: true,
                portfolio_id: Some(PORTFOLIO.into()),
                product: CoinbaseProduct::Advanced,
            }],
            BTreeMap::from([("synthetic".into(), api.clone() as Arc<dyn CoinbaseApi>)]),
        )
        .unwrap()
        .with_prices(Arc::new(TestPrices))
        .with_recipient_verifier(Arc::new(SettlementRecipient(destination_for(route))));
        let mut input = withdraw_input();
        input["network"] = json!(route.network);
        let prepared = profile.prepare("coinbase.withdraw", &input).await.unwrap();
        let planned = profile.plan_submission(&prepared).await.unwrap();
        *api.idem.lock().unwrap() = planned.submission.reference.clone();
        // Reload the immutable original journal shape with no dispatch receipt:
        // status must locate exactly the saved idem, not create another send.
        let record: gofin_domain::OperationRecord = serde_json::from_value(json!({
            "id":"original-operation","request_id":"withdrawal-test","fingerprint":"synthetic","fingerprint_version":1,"operation":"coinbase.withdraw","input":input,"status":"unknown","created_at":gofin_domain::now(),"updated_at":gofin_domain::now(),"preview":prepared,"approval":null,"submission":planned.submission,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null
        })).unwrap();
        let submission = record.submission.as_ref().unwrap();
        let result = profile
            .reconcile_submission(&record, submission)
            .await
            .unwrap();
        assert_eq!(
            result.status,
            gofin_domain::Status::Completed,
            "{}: {:?}",
            route.network,
            result.evidence
        );
        let effects = result.verified.unwrap();
        assert_eq!(&effects.submission, submission);
        assert!(effects.effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::Transfer { credit, .. } if credit.asset.network == format!("ethereum:{}",route.chain_id) && credit.asset.reference == route.contract)));
        *api.network.lock().unwrap() = if route.network == "base" {
            "ethereum"
        } else {
            "base"
        }
        .into();
        let mismatch = profile
            .reconcile_submission(&record, submission)
            .await
            .unwrap();
        assert_eq!(mismatch.status, gofin_domain::Status::Unknown);
        assert!(mismatch.verified.is_none());
    }
}
