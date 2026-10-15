use crate::{KrakenEndpoint, inputs};
use gofin_domain::{Definition, ExecutionActor, GoFinError, OperationKind, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Inventory {
    endpoints: Vec<Contract>,
}
#[derive(Deserialize)]
pub struct Contract {
    pub client: String,
    pub method: String,
    pub operation: Option<String>,
    pub schema: Option<Value>,
    #[serde(default)]
    pub mutating: bool,
    #[serde(default)]
    pub administrative: bool,
    pub excluded: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub deprecated: bool,
}
/// Canonical representation of a nonsecret venue account identifier. This does
/// not equate products or infer a relationship between a UUID and an IIBAN.
pub fn canonical_account_identity(identity: &str) -> String {
    let compact: String = identity.chars().filter(|c| !c.is_whitespace()).collect();
    uuid::Uuid::parse_str(&compact)
        .map(|uid| uid.to_string())
        .unwrap_or_else(|_| {
            if compact.len() == 16 && compact.bytes().all(|c| c.is_ascii_alphanumeric()) {
                compact.to_ascii_uppercase()
            } else {
                compact
            }
        })
}

pub fn contracts() -> &'static [Contract] {
    static INVENTORY: OnceLock<Inventory> = OnceLock::new();
    &INVENTORY
        .get_or_init(|| {
            serde_json::from_str(include_str!("../../../docs/kraken/contracts.json"))
                .expect("Kraken contract snapshot is valid")
        })
        .endpoints
}
pub fn contract(endpoint: KrakenEndpoint) -> &'static Contract {
    contracts()
        .iter()
        .find(|c| c.method == endpoint.method())
        .expect("closed endpoint has contract")
}
pub fn endpoint(operation: &str) -> Result<KrakenEndpoint> {
    KrakenEndpoint::ALL
        .iter()
        .copied()
        .find(|e| e.operation() == operation)
        .ok_or_else(|| GoFinError::validation("Unknown typed Kraken operation."))
}

/// GoFin-owned controls are separate from the operation-specific venue DTO.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AccountSelection {
    pub account: Option<String>,
}

pub fn decode(endpoint: KrakenEndpoint, input: &Value) -> Result<(AccountSelection, Value)> {
    validate_gofin_controls(endpoint, input, None)?;
    let mut fields = input
        .as_object()
        .cloned()
        .ok_or_else(|| GoFinError::validation("Kraken input must be an object."))?;
    let account = match fields.remove("account") {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        None => None,
        _ => {
            return Err(GoFinError::validation(
                "Select an enrolled Kraken account by name.",
            ));
        }
    };
    if endpoint.private() && account.is_none() {
        return Err(GoFinError::validation(
            "This Kraken operation requires an account.",
        ));
    }
    if !endpoint.private() && account.is_some() {
        return Err(GoFinError::validation(
            "Public Kraken data does not use credentials.",
        ));
    }
    let mut raw = Value::Object(fields);
    let controls = financial_controls(endpoint);
    let schema = gofin_input_schema(endpoint);
    let validator = jsonschema::validator_for(&schema)
        .map_err(|_| GoFinError::validation("Kraken contract schema is invalid."))?;
    if !validator.is_valid(&raw) {
        return Err(GoFinError::new(
            "INVALID_INPUT",
            "validation",
            "Input does not match this Kraken operation's exact schema.",
            "Inspect gofin schema for the selected operation; fields and quantities are operation-specific.",
        ));
    }
    for name in controls.keys() {
        raw.as_object_mut().expect("object").remove(name);
    }
    Ok((
        AccountSelection { account },
        inputs::decode_wire(endpoint, raw)?,
    ))
}

/// Validate adapter-internal reads against the same closed native schema.
/// This prevents mocked transports from masking wrong enums or parameter names.
pub(crate) fn validate_native(endpoint: KrakenEndpoint, parameters: &Value) -> Result<()> {
    type Cell = (KrakenEndpoint, OnceLock<Option<jsonschema::Validator>>);
    static VALIDATORS: OnceLock<Vec<Cell>> = OnceLock::new();
    let cells = VALIDATORS.get_or_init(|| {
        KrakenEndpoint::ALL
            .iter()
            .map(|endpoint| (*endpoint, OnceLock::new()))
            .collect()
    });
    let cell = &cells
        .iter()
        .find(|(selected, _)| *selected == endpoint)
        .expect("closed endpoint validator")
        .1;
    let validator = cell
        .get_or_init(|| {
            jsonschema::validator_for(
                contract(endpoint)
                    .schema
                    .as_ref()
                    .expect("active native schema"),
            )
            .ok()
        })
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Kraken native schema is invalid."))?;
    if let Err(failure) = validator.validate(parameters) {
        return Err(GoFinError::new(
            "KRAKEN_INTERNAL_REQUEST_INVALID",
            "adapter",
            "The adapter request does not match the selected native Kraken contract.",
            "Report this operation-specific mapping failure; no request was dispatched.",
        )
        .with_details(
            json!({"operation":endpoint.operation(),"field":failure.instance_path().to_string()}),
        ));
    }
    Ok(())
}

/// Extra GoFin approval controls never become venue request parameters.
pub fn financial_controls(endpoint: KrakenEndpoint) -> serde_json::Map<String, Value> {
    let c = contract(endpoint);
    let mut result = serde_json::Map::new();
    if !c.mutating || c.administrative {
        return result;
    }
    let decimal = json!({"type":"string","pattern":"^(?:0|[1-9][0-9]*)(?:\\.[0-9]+)?$"});
    result.insert(
        "requestId".into(),
        json!({"type":"string","minLength":1,"maxLength":128}),
    );
    let funding = c.method.contains("withdraw")
        || c.method.contains("earn_")
        || c.method.contains("transfer")
        || c.method.contains("deposit_address");
    if !funding {
        if (c.method.contains("cancel") && !c.method.contains("after"))
            || c.method.contains("amend")
            || c.method.contains("edit")
            || endpoint == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
        {
            result.insert("operationId".into(), json!({"type":"string","minLength":1}));
            result.insert(
                "originalSubmission".into(),
                serde_json::to_value(schemars::schema_for!(gofin_domain::Submission))
                    .expect("submission schema"),
            );
        }
        if order_controls(endpoint) {
            result.insert("maximumFees".into(), json!({"type":"object","propertyNames":{"type":"string","minLength":1},"additionalProperties":decimal,"maxProperties":32}));
            for name in ["maximumLoss", "maximumProfit"] {
                result.insert(name.into(), decimal.clone());
            }
            for name in if endpoint.futures() {
                ["maximumFundingLoss", "maximumFundingProfit"]
            } else {
                ["maximumSpend", "minimumReceive"]
            } {
                result.insert(name.into(), decimal.clone());
            }
        }
        if endpoint == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder {
            let mut child_limits = serde_json::Map::new();
            for name in [
                "maximumFees",
                "maximumLoss",
                "maximumProfit",
                "maximumFundingLoss",
                "maximumFundingProfit",
            ] {
                child_limits.insert(name.into(), result[name].clone());
            }
            result.insert("amendments".into(), json!({"type":"array","minItems":1,"maxItems":50,"items":{
                "type":"object","required":["orderTag","operationId","originalSubmission"],"properties":{
                    "orderTag":{"type":"string","minLength":1,"maxLength":128},
                    "operationId":{"type":"string","minLength":1},
                    "originalSubmission":serde_json::to_value(schemars::schema_for!(gofin_domain::Submission)).expect("submission schema"),
                    "limits":{"type":"object","properties":child_limits,"additionalProperties":false}
                },"additionalProperties":false
            }}));
            let bounds = json!({"type":"object","required":["maximumLoss","maximumProfit","maximumFundingLoss","maximumFundingProfit"],"properties":{
                "maximumLoss":decimal.clone(),"maximumProfit":decimal.clone(),
                "maximumFundingLoss":decimal.clone(),"maximumFundingProfit":decimal.clone()
            },"additionalProperties":false});
            result.insert(
                "settlementLimits".into(),
                json!({
                    "type":"object","propertyNames":{"type":"string","minLength":1,"maxLength":128},
                    "additionalProperties":bounds,"minProperties":1,"maxProperties":32
                }),
            );
        }
    }
    if matches!(
        endpoint,
        KrakenEndpoint::KrakenPrivatePostWithdraw
            | KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals
    ) {
        result.insert("verificationRequirement".into(),json!({"type":"string","enum":["destination_receipt","venue_only"],"default":"destination_receipt"}));
        result.insert("destination".into(), json!({"type":"string","minLength":1}));
    }
    if c.method.contains("transfer")
        || endpoint == KrakenEndpoint::KrakenfuturesPrivatePostWithdrawal
    {
        result.insert(
            "destinationAccount".into(),
            json!({"type":"string","minLength":1}),
        );
    }
    if endpoint == KrakenEndpoint::KrakenfuturesPrivatePostTransferSubaccount {
        result.insert(
            "sourceAccount".into(),
            json!({"type":"string","minLength":1}),
        );
    }
    if matches!(
        endpoint,
        KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals
            | KrakenEndpoint::KrakenPrivatePostEarnAllocate
            | KrakenEndpoint::KrakenPrivatePostEarnDeallocate
            | KrakenEndpoint::KrakenFundingPutFundingV1DepositAddress
            | KrakenEndpoint::KrakenLegacyCreateDepositAddress
    ) {
        result.insert("maxFee".into(), decimal);
    }
    result
}

fn order_controls(endpoint: KrakenEndpoint) -> bool {
    use KrakenEndpoint as E;
    matches!(
        endpoint,
        E::KrakenPrivatePostAddOrder
            | E::KrakenPrivatePostAddOrderBatch
            | E::KrakenPrivatePostAmendOrder
            | E::KrakenPrivatePostEditOrder
            | E::KrakenfuturesPrivatePostSendorder
            | E::KrakenfuturesPrivatePostEditorder
            | E::KrakenfuturesPrivatePostBatchorder
    )
}

pub(crate) const MONETARY_CONTROLS: &[&str] = &[
    "maximumFees",
    "maximumSpend",
    "minimumReceive",
    "maximumLoss",
    "maximumProfit",
    "maximumFundingLoss",
    "maximumFundingProfit",
    "settlementLimits",
];

pub(crate) fn unsupported_control(endpoint: KrakenEndpoint, fields: Vec<String>) -> GoFinError {
    GoFinError::new("KRAKEN_CONTROL_UNSUPPORTED", "input",
        "These GoFin limits do not apply to the selected Kraken product or operation.",
        "Remove the listed fields and inspect this operation's schema; unsupported limits never authorize a venue request.")
        .with_details(json!({"product":contract(endpoint).client,"operation":endpoint.operation(),"fields":fields}))
}

/// Internal batch projections deliberately pass monetary authority only to
/// financial children. The public batch retains and validates its own limits.
pub(crate) fn remove_monetary_controls(input: &mut Value) {
    if let Some(fields) = input.as_object_mut() {
        for key in MONETARY_CONTROLS {
            fields.remove(*key);
        }
    }
}

fn absent_fields(fields: &[&str]) -> Value {
    json!({"not":{"anyOf":fields.iter().map(|field|json!({"required":[field]})).collect::<Vec<_>>()}})
}
fn gofin_input_schema(endpoint: KrakenEndpoint) -> Value {
    use KrakenEndpoint as E;
    let mut schema = contract(endpoint)
        .schema
        .as_ref()
        .expect("active native schema")
        .clone();
    let controls = financial_controls(endpoint);
    if !controls.is_empty() {
        schema["properties"]
            .as_object_mut()
            .unwrap()
            .extend(controls);
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("requestId"));
    }
    let mut conditions = vec![];
    if schema["properties"].get("validate").is_some() && order_controls(endpoint) {
        let mut fields = MONETARY_CONTROLS.to_vec();
        fields.extend(["operationId", "originalSubmission"]);
        conditions.push(json!({"if":{"properties":{"validate":{"const":true}},"required":["validate"]},"then":absent_fields(&fields)}));
    }
    if matches!(
        endpoint,
        E::KrakenPrivatePostAddOrder | E::KrakenPrivatePostAddOrderBatch
    ) {
        let leveraged = json!({"anyOf":[{"properties":{"leverage":{"not":{"enum":["none","1"]}}},"required":["leverage"]},{"properties":{"ordertype":{"const":"settle-position"}},"required":["ordertype"]}]});
        let margin = if endpoint == E::KrakenPrivatePostAddOrderBatch {
            json!({"properties":{"orders":{"contains":leveraged}},"required":["orders"]})
        } else {
            leveraged
        };
        conditions.push(json!({"if":margin,"then":absent_fields(&["minimumReceive"]),"else":absent_fields(&["maximumLoss","maximumProfit"])}));
    }
    if endpoint == E::KrakenfuturesPrivatePostBatchorder {
        conditions.push(json!({"if":{"properties":{"batchOrder":{"minItems":1,"items":{"properties":{"order":{"const":"cancel"}},"required":["order"]}}},"required":["batchOrder"]},"then":absent_fields(MONETARY_CONTROLS)}));
        conditions.push(json!({"if":{"required":["settlementLimits"]},"then":absent_fields(&["maximumLoss","maximumProfit","maximumFundingLoss","maximumFundingProfit"])}));
    }
    if !conditions.is_empty() {
        let existing = schema
            .as_object_mut()
            .unwrap()
            .entry("allOf")
            .or_insert_with(|| json!([]));
        existing.as_array_mut().unwrap().extend(conditions);
    }
    schema
}

/// Validate family-specific GoFin authority before any venue reads. Protected
/// amendment scopes supply product meaning; native inputs cannot invent it.
pub(crate) fn validate_gofin_controls(
    endpoint: KrakenEndpoint,
    input: &Value,
    original_binding: Option<&Value>,
) -> Result<()> {
    use KrakenEndpoint as E;
    let allowed = financial_controls(endpoint);
    let mut invalid = Vec::new();
    for key in MONETARY_CONTROLS
        .iter()
        .copied()
        .chain(["operationId", "originalSubmission"])
    {
        if input.get(key).is_some() && !allowed.contains_key(key) {
            invalid.push(format!("/{key}"));
        }
    }
    if input["validate"] == true {
        for key in MONETARY_CONTROLS
            .iter()
            .copied()
            .chain(["operationId", "originalSubmission"])
        {
            if input.get(key).is_some() {
                invalid.push(format!("/{key}"));
            }
        }
    }
    if endpoint == E::KrakenfuturesPrivatePostBatchorder {
        let rows = input["batchOrder"].as_array();
        if rows
            .is_some_and(|rows| !rows.is_empty() && rows.iter().all(|row| row["order"] == "cancel"))
        {
            for key in MONETARY_CONTROLS {
                if input.get(*key).is_some() {
                    invalid.push(format!("/{key}"));
                }
            }
        }
        if input.get("settlementLimits").is_some() {
            for key in [
                "maximumLoss",
                "maximumProfit",
                "maximumFundingLoss",
                "maximumFundingProfit",
            ] {
                if input.get(key).is_some() {
                    invalid.push(format!("/{key}"));
                }
            }
        }
        for (index, amendment) in input["amendments"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let cancellation = rows
                .into_iter()
                .flatten()
                .any(|row| row["order_tag"] == amendment["orderTag"] && row["order"] == "cancel");
            for key in amendment["limits"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(key, _)| key)
            {
                if cancellation || matches!(key.as_str(), "maximumSpend" | "minimumReceive") {
                    invalid.push(format!("/amendments/{index}/limits/{key}"));
                }
            }
        }
    }
    // An amendment's cash/margin meaning comes from its immutable owner, not
    // optional native fields which the amendment endpoint does not expose.
    if !endpoint.futures() && order_controls(endpoint) && input["validate"] != true {
        let amendment = matches!(
            endpoint,
            E::KrakenPrivatePostAmendOrder | E::KrakenPrivatePostEditOrder
        );
        let margin = if amendment {
            original_binding.map(|binding| {
                binding["margin"].is_object() || binding["mixedSpotMargin"].is_object()
            })
        } else {
            Some(if endpoint == E::KrakenPrivatePostAddOrderBatch {
                input["orders"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(crate::mixed_spot_margin::leveraged))
            } else {
                crate::mixed_spot_margin::leveraged(input)
            })
        };
        match margin {
            Some(false) => {
                for key in ["maximumLoss", "maximumProfit"] {
                    if input.get(key).is_some() {
                        invalid.push(format!("/{key}"));
                    }
                }
            }
            Some(true)
                if original_binding
                    .is_none_or(|binding| !binding["mixedSpotMargin"].is_object()) =>
            {
                if input.get("minimumReceive").is_some() {
                    invalid.push("/minimumReceive".into());
                }
                let closing_only = if amendment {
                    original_binding
                        .and_then(|binding| binding["margin"]["orders"].as_array())
                        .is_some_and(|orders| {
                            !orders.is_empty()
                                && orders.iter().all(|order| order["closing"] == true)
                        })
                } else if endpoint == E::KrakenPrivatePostAddOrderBatch {
                    input["orders"].as_array().is_some_and(|orders| {
                        !orders.is_empty()
                            && orders.iter().all(|order| {
                                order["reduce_only"] == true
                                    || order["ordertype"] == "settle-position"
                            })
                    })
                } else {
                    input["reduce_only"] == true || input["ordertype"] == "settle-position"
                };
                if closing_only && input.get("maximumSpend").is_some() {
                    invalid.push("/maximumSpend".into());
                }
                let settling = if endpoint == E::KrakenPrivatePostAddOrderBatch {
                    input["orders"].as_array().is_some_and(|orders| {
                        !orders.is_empty()
                            && orders
                                .iter()
                                .all(|order| order["ordertype"] == "settle-position")
                    })
                } else {
                    input["ordertype"] == "settle-position"
                };
                if settling {
                    for key in ["maximumLoss", "maximumProfit"] {
                        if input.get(key).is_some() {
                            invalid.push(format!("/{key}"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    invalid.sort();
    invalid.dedup();
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(unsupported_control(endpoint, invalid))
    }
}

fn output_schema(endpoint: KrakenEndpoint) -> Value {
    let c = contract(endpoint);
    if c.administrative {
        return json!({"type":"object","required":["id","requestId","operation","status","account","product","verification","data","observation","nextAction"],"properties":{"id":{"type":"string"},"requestId":{"type":"string"},"operation":{"const":endpoint.operation()},"status":{"enum":["unapproved","dispatch_started","unknown","acknowledged"]},"account":{"type":"string"},"product":{"const":c.client},"verification":{"type":"object"},"data":{},"observation":{},"nextAction":{"type":"string"}},"additionalProperties":false});
    }
    if c.mutating {
        return serde_json::to_value(schemars::schema_for!(gofin_domain::OperationRecord))
            .expect("operation record schema");
    }
    if c.method.contains("retrieve_export") || c.method.contains("accountlogcsv") {
        return json!({"type":"object","required":["provider","operation","output","bytes","sha256","contentType"],"properties":{"provider":{"const":c.client},"operation":{"const":endpoint.operation()},"output":{"type":"string"},"bytes":{"type":"integer","minimum":0,"maximum":33554432},"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},"contentType":{"type":"string"}},"additionalProperties":false});
    }
    json!({"type":"object","required":["provider","operation","observedAt","data"],"properties":{"provider":{"const":c.client},"operation":{"const":endpoint.operation()},"observedAt":{"type":"string"},"data":{},"coverage":{"type":"string"}},"additionalProperties":false})
}

fn examples(endpoint: KrakenEndpoint) -> Vec<Value> {
    use KrakenEndpoint as E;
    let example = match endpoint {
        E::KrakenPublicGetTime => json!({}),
        E::KrakenPublicGetAssetPairs => json!({"pair":"XBTUSD"}),
        E::KrakenPublicGetTicker | E::KrakenPublicGetDepth => json!({"pair":"XBTUSD"}),
        E::KrakenPrivatePostBalance
        | E::KrakenPrivatePostBalanceEx
        | E::KrakenPrivatePostGetApiKeyInfo
        | E::KrakenPrivatePostListWalletAccounts => json!({"account":"spot"}),
        E::KrakenPrivatePostAddOrder => {
            json!({"account":"spot","requestId":"buy-xbt-1","pair":"XBTUSD","type":"buy","ordertype":"limit","volume":"0.001","price":"20000","timeinforce":"IOC","maximumFees":{"ZUSD":"0.2","XXBT":"0.00001"}})
        }
        E::KrakenPrivatePostCancelAllOrdersAfter => {
            json!({"account":"spot","requestId":"timer-1","timeout":60})
        }
        E::KrakenPrivatePostRetrieveExport => json!({"account":"spot","id":"YOUR_EXPORT_ID"}),
        E::KrakenfuturesPrivateGetAccounts
        | E::KrakenfuturesPrivateGetOpenpositions
        | E::KrakenfuturesAuthGetApiKeysV3Check => json!({"account":"derivatives"}),
        E::KrakenfuturesPublicGetInstruments => json!({}),
        E::KrakenfuturesPrivatePutLeveragepreferences => {
            json!({"account":"derivatives","symbol":"PI_XBTUSD","maxLeverage":"2"})
        }
        E::KrakenFundingGetFundingV1Networks => json!({"account":"spot"}),
        _ => return vec![],
    };
    vec![example]
}

pub fn definitions(client: &str) -> Vec<Definition> {
    KrakenEndpoint::ALL.iter().filter(|e|contract(**e).client==client).map(|e| {
        let c=contract(*e);let mut schema=gofin_input_schema(*e);
        if e.private() {schema["properties"]["account"]=json!({"type":"string","minLength":1});schema["required"].as_array_mut().expect("required list").push(json!("account"));}
        Definition{name:e.operation().into(),description:c.description.clone(),
            kind:if c.administrative{OperationKind::Configuration}else if c.mutating{OperationKind::Write}else{OperationKind::Read},
            execution_actor:if c.administrative{ExecutionActor::HumanInteractive}else if c.mutating{ExecutionActor::AgentInvocable}else{ExecutionActor::AgentReadable},
            input_schema:schema,
            output_schema:output_schema(*e),
            error_schema:serde_json::to_value(schemars::schema_for!(GoFinError)).expect("error schema"),
            examples:examples(*e),coverage:if c.deprecated{"Deprecated venue endpoint; synthetic coverage is separate from live verification."}else{"Typed REST contract; consult the per-capability evidence matrix for verification."}.into(),
            completion:if c.mutating{"Verified operation-specific effects, never HTTP acknowledgment alone."}else{"An exact bounded venue response with its account/product semantics."}.into(),
            semantics:json!({"broker_generated_attempt_input_fields":if c.mutating&&!c.administrative{json!(["/requestId"])}else{json!([])},"ccxtVersion":"4.5.85","ccxtMethod":c.method,"product":c.client,"administrative":c.administrative,"deprecated":c.deprecated,"implementationStatus":if c.administrative{"configuration_implemented"}else if c.mutating{"financial_handler_verification_in_progress"}else if c.method.contains("retrieve_export")||c.method.contains("accountlogcsv"){"artifact_read_implemented"}else{"read_implemented"},"financial_authority":if c.administrative{"separate configuration approval"}else if c.mutating{"protected financial approval"}else{"none"}}),
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_primitive_unions_accept_decimal_text_without_overlapping_oneof_failure() {
        let (_,address)=decode(KrakenEndpoint::KrakenPrivatePostDepositAddresses,&json!({"account":"spot","asset":"XXBT","method":"Bitcoin Lightning","amount":"0.000000000000000001"})).unwrap();
        assert_eq!(address["amount"], "0.000000000000000001");
        let (_, fees) = decode(
            KrakenEndpoint::KrakenPrivatePostTradeVolume,
            &json!({"account":"spot","fee-info":"1.0"}),
        )
        .unwrap();
        assert_eq!(fees["fee-info"], "1.0");
        validate_native(KrakenEndpoint::KrakenPrivatePostTradeVolume, &fees).unwrap();
    }
    #[test]
    fn pinned_inventory_and_closed_endpoints_are_exhaustive() {
        for client in ["kraken", "krakenfutures"] {
            let raw = if client == "kraken" {
                include_str!("../../../docs/kraken/kraken-ccxt.json")
            } else {
                include_str!("../../../docs/kraken/krakenfutures-ccxt.json")
            };
            let source: Value = serde_json::from_str(raw).unwrap();
            for method in source["implicitMethods"].as_array().unwrap() {
                let c = contracts()
                    .iter()
                    .find(|c| c.client == client && c.method == method.as_str().unwrap())
                    .expect("every source endpoint recorded");
                assert!(
                    c.excluded.is_some()
                        || KrakenEndpoint::ALL.iter().any(|e| e.method() == c.method)
                );
            }
            for (capability, binding) in source["capabilityBindings"].as_object().unwrap() {
                assert!(
                    !binding["operations"].as_array().unwrap().is_empty(),
                    "supported capability {capability} must map explicitly"
                );
                for operation in binding["operations"].as_array().unwrap() {
                    assert!(
                        endpoint(operation.as_str().unwrap()).is_ok(),
                        "{capability}: {operation}"
                    );
                }
            }
        }
        assert_eq!(KrakenEndpoint::ALL.len(), 131);
        let names: std::collections::BTreeSet<_> =
            KrakenEndpoint::ALL.iter().map(|e| e.operation()).collect();
        assert_eq!(names.len(), 131);
    }
    #[test]
    fn every_generated_schema_compiles_and_keeps_native_options_closed() {
        for endpoint in KrakenEndpoint::ALL {
            let definition = definitions(&contract(*endpoint).client)
                .into_iter()
                .find(|d| d.name == endpoint.operation())
                .unwrap();
            jsonschema::validator_for(&definition.input_schema)
                .unwrap_or_else(|e| panic!("{}: {e}", definition.name));
            jsonschema::validator_for(&definition.output_schema)
                .unwrap_or_else(|e| panic!("{}: {e}", definition.name));
            assert_eq!(definition.input_schema["additionalProperties"], false);
        }
    }
    #[test]
    fn funding_fee_union_and_explicit_nullable_values_survive_exact_wire_decode() {
        let mut input = json!({"account":"synthetic-spot","requestId":"synthetic-withdrawal","scope":{"method_id":"00000000-0000-0000-0000-000000000001"},"address_id":"ABAAAAA-AAAAA-AAAAAA","amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1.000000000000000000000000000000000001"}},"fee":{"fee_included":true,"current_fee":{"max_fee":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"0.01"}}}}});
        let (_, wire) = decode(
            KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,
            &input,
        )
        .unwrap();
        assert_eq!(wire["fee"], input["fee"]);
        assert_eq!(wire["amount"], input["amount"]);
        assert!(wire.get("requestId").is_none());
        input["fee"] = json!({"fee_included":false,"quoted_fee":{"token":"synthetic-public-fee-quote-id","rebase_multiplier":"base"}});
        assert_eq!(
            decode(
                KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,
                &input
            )
            .unwrap()
            .1["fee"],
            input["fee"]
        );
        input["fee"]["current_fee"] = json!({});
        assert!(
            decode(
                KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,
                &input
            )
            .is_err()
        );
        let (_, nullable) = decode(
            KrakenEndpoint::KrakenPrivatePostEarnAllocations,
            &json!({"account":"synthetic-spot","converted_asset":null}),
        )
        .unwrap();
        assert!(nullable.get("converted_asset").is_some());
        assert_eq!(nullable["converted_asset"], Value::Null);
        assert!(
            decode(
                KrakenEndpoint::KrakenPrivatePostEarnAllocations,
                &json!({"account":"synthetic-spot"})
            )
            .unwrap()
            .1
            .get("converted_asset")
            .is_none()
        );
    }
    #[test]
    fn reads_are_strict_and_do_not_mutate_address_state() {
        assert!(
            decode(
                KrakenEndpoint::KrakenPrivatePostBalance,
                &json!({"account":"spot"})
            )
            .is_ok()
        );
        assert!(
            decode(
                KrakenEndpoint::KrakenPrivatePostBalance,
                &json!({"account":"spot","nonce":"1"})
            )
            .is_err()
        );
        assert!(
            decode(
                KrakenEndpoint::KrakenPrivatePostDepositAddresses,
                &json!({"account":"spot","asset":"ETH","method":"Ether","new":true})
            )
            .is_err()
        );
    }
    #[test]
    fn api_keys_and_private_financial_inputs_are_not_discoverable_secrets() {
        for d in definitions("kraken")
            .into_iter()
            .chain(definitions("krakenfutures"))
        {
            let properties = d.input_schema["properties"].as_object().unwrap();
            for forbidden in ["nonce", "otp", "apiKey", "secret", "endpoint", "params"] {
                assert!(
                    !properties.contains_key(forbidden),
                    "{} {forbidden}",
                    d.name
                );
            }
        }
    }
    #[test]
    fn financial_and_administrative_mutations_never_use_read_classification() {
        for method in [
            "private_post_add_order",
            "private_post_amend_order",
            "private_post_cancel_order",
            "private_post_add_order_batch",
            "private_post_withdraw",
            "private_post_withdraw_cancel",
            "private_post_create_subaccount",
            "private_post_earn_allocate",
            "private_post_earn_deallocate",
            "private_post_sendorder",
            "private_post_batchorder",
            "private_post_cancelallordersafter",
            "private_put_leveragepreferences",
            "private_put_subaccount_uid_trading_enabled",
            "funding_post_funding_v1_withdrawals",
            "funding_put_funding_v1_deposit_address",
            "funding_post_funding_v1_addresses",
        ] {
            let endpoint = KrakenEndpoint::ALL
                .iter()
                .find(|e| e.method() == method)
                .expect("curated mutation exists");
            assert!(endpoint.mutating(), "{method}");
            assert!(contract(*endpoint).mutating, "{method}");
        }
        assert!(!KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus.mutating());
        assert!(!KrakenEndpoint::KrakenPrivatePostDepositAddresses.mutating());
    }
    #[test]
    fn futures_batch_is_structural_and_rejects_raw_json_passthrough() {
        let endpoint = KrakenEndpoint::KrakenfuturesPrivatePostBatchorder;
        assert!(
            decode(
                endpoint,
                &json!({"account":"derivatives","json":"{\"batchOrder\":[]}"})
            )
            .is_err()
        );
        assert!(decode(endpoint,&json!({"account":"derivatives","requestId":"synthetic-1","batchOrder":[{"order":"send","order_tag":"synthetic-1","symbol":"PI_XBTUSD","orderType":"lmt","side":"buy","size":"1","limitPrice":"10"}]})).is_ok());
        assert!(decode(endpoint,&json!({"account":"derivatives","requestId":"synthetic-1","batchOrder":[{"order":"send","order_tag":"synthetic-1","symbol":"PI_XBTUSD","orderType":"lmt","side":"buy","size":"1","limitPrice":"10","unknownPermissionOverride":true}]})).is_err());
    }
    #[test]
    fn examples_match_the_exact_public_input_contracts() {
        for client in ["kraken", "krakenfutures"] {
            for definition in definitions(client) {
                let validator = jsonschema::validator_for(&definition.input_schema).unwrap();
                for example in definition.examples {
                    assert!(
                        validator.is_valid(&example),
                        "{}: {}",
                        definition.name,
                        example
                    );
                }
            }
        }
    }

    #[test]
    fn every_financial_endpoint_has_a_protected_lifecycle_handler() {
        for endpoint in KrakenEndpoint::ALL {
            let c = contract(*endpoint);
            if c.mutating && !c.administrative {
                assert!(
                    crate::trading::is_trading(*endpoint) || crate::funding::is_funding(*endpoint),
                    "Missing protected handler: {}",
                    endpoint.operation()
                );
            }
        }
    }
}
