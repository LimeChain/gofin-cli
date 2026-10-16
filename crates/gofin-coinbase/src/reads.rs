//! Typed discovery for the pinned native GET inventory.
use gofin_domain::{Definition, ExecutionActor, OperationKind};
use gofin_exchange::coinbase::{
    CoinbaseQuery, CoinbaseRoute,
    reads::{READS, ReadDescriptor},
};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub const MATRIX_JSON: &str = include_str!("../../../docs/coinbase/contracts.json");

fn matrix_version() -> &'static Value {
    static VERSION: OnceLock<Value> = OnceLock::new();
    VERSION.get_or_init(|| {
        serde_json::from_str::<Value>(MATRIX_JSON)
            .expect("reviewed matrix")
            .get("matrix_version")
            .cloned()
            .expect("reviewed matrix version")
    })
}

/// Keep native decimal lexemes and expose only the reviewed cursor headers.
pub(crate) fn response_value(
    response: &gofin_exchange::coinbase::CoinbaseResponse,
) -> Result<Value, serde_json::Error> {
    let data: Value = serde_json::from_slice(&response.body)?;
    if response.pagination.after.is_some() || response.pagination.before.is_some() {
        Ok(
            json!({"data":data,"pagination":{"after":response.pagination.after,"before":response.pagination.before}}),
        )
    } else {
        Ok(data)
    }
}

pub fn descriptor(operation: &str) -> Option<&'static ReadDescriptor> {
    READS
        .iter()
        .filter(|d| d.path != "/v2/users/{user_id}")
        .find(|d| d.operation == operation)
}
pub fn schema(d: &ReadDescriptor) -> Value {
    let text = json!({"type":"string","minLength":1,"maxLength":256,"pattern":"^[^\\u0000-\\u001f\\u007f]+$"});
    let mut fields = serde_json::Map::new();
    fields.insert("account".into(), text.clone());
    let mut required = vec!["account"];
    for key in d.parameters {
        fields.insert((*key).into(), text.clone());
        required.push(key);
    }
    let mut query = serde_json::Map::new();
    for (key, _) in d.query {
        query.insert((*key).into(),match *key {
 "limit"=>json!({"type":"integer","minimum":1,"maximum":if d.path.ends_with("/candles") {350} else {1000}}),
 "offset"=>json!({"type":"integer","minimum":0,"maximum":4294967295_u64}),
 "level"=>json!({"type":"integer","enum":[1,2,3]}),
 "product_type"=>json!({"type":"string","enum":["SPOT","FUTURE"]}),
 "granularity" if d.product=="coinbase"=>json!({"type":"string","enum":["ONE_MINUTE","FIVE_MINUTE","FIFTEEN_MINUTE","THIRTY_MINUTE","ONE_HOUR","TWO_HOUR","FOUR_HOUR","SIX_HOUR","ONE_DAY"]}),
 "granularity"=>json!({"type":"integer","enum":[60,300,900,3600,21600,86400]}),
 "start"|"end" if d.product=="coinbase"=>json!({"type":"string","pattern":"^[0-9]{1,20}$"}),
 "date"=>json!({"type":"string","pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}$"}),
 "order_ids"|"product_ids"=>json!({"type":"array","items":text,"maxItems":100,"uniqueItems":true}),
 _=>text.clone()
 });
    }
    if !query.is_empty() {
        let required_query = if d.product == "coinbase" && d.path.ends_with("/candles") {
            vec!["start", "end", "granularity"]
        } else if d.path.ends_with("product_book") {
            vec!["product_id"]
        } else {
            vec![]
        };
        if !required_query.is_empty() {
            required.push("query");
        }
        fields.insert("query".into(),json!({"type":"object","properties":query,"required":required_query,"additionalProperties":false}));
    }
    json!({"type":"object","properties":fields,"required":required,"additionalProperties":false})
}
pub fn definitions() -> Vec<Definition> {
    let matrix_version = matrix_version();
    READS.iter().filter(|d| d.path != "/v2/users/{user_id}").map(|d|Definition {
 name:d.operation.into(),description:format!("Read {} GET {} through the pinned Coinbase REST contract.",d.product,d.path),
 kind:OperationKind::Read,execution_actor:ExecutionActor::AgentReadable,input_schema:schema(d),output_schema:json!({}),error_schema:json!({"type":"object"}),
 examples:vec![],coverage:"One native response page; follow the native cursor/pagination fields. Account restrictions and retired server routes may reject a read.".into(),
 completion:"A read response is an observation; it does not establish financial settlement.".into(),
 semantics:json!({"matrixVersion":matrix_version,"product":d.product,"method":"GET","nativePath":d.path,"permission":if d.public {"public"} else {"view"},"clientVersion":"ccxt 4.5.85","pagination":"explicit","optionsCoverage":"declared-query-subset"})
 }).collect()
}
pub fn endpoint(d: &ReadDescriptor, input: &Value) -> CoinbaseRoute {
    let q = &input["query"];
    let text = |key: &str| q[key].as_str().map(str::to_owned);
    let list = |key: &str| {
        q[key]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    CoinbaseRoute::NativeRead {
        route: d.route,
        parameters: d
            .parameters
            .iter()
            .map(|key| input[key].as_str().expect("schema validated").to_owned())
            .collect(),
        query: CoinbaseQuery {
            offset: q["offset"].as_u64().map(|n| n as u32),
            level: q["level"].as_u64().map(|n| n as u8),
            granularity: text("granularity")
                .or_else(|| q["granularity"].as_u64().map(|n| n.to_string())),
            start: text("start"),
            end: text("end"),
            currency: text("currency"),
            date: text("date"),
            product_type: text("product_type"),
            limit: q["limit"].as_u64().map(|n| n as u16),
            cursor: text("cursor"),
            starting_after: text("starting_after"),
            ending_before: text("ending_before"),
            product_id: text("product_id"),
            order_id: text("order_id"),
            status: text("status"),
            start_time: text("start_time"),
            end_time: text("end_time"),
            order_ids: list("order_ids"),
            product_ids: list("product_ids"),
            ..Default::default()
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unified_method_matrix_preserves_pinned_flags_and_valid_native_links() {
        let matrix: Value = serde_json::from_str(MATRIX_JSON).unwrap();
        let pinned: Value = serde_json::from_str(include_str!(
            "../../../docs/coinbase/pinned-ccxt-4.5.85.json"
        ))
        .unwrap();
        for (name, product) in matrix["products"].as_object().unwrap() {
            let endpoints = product["implicit_endpoints"].as_array().unwrap();
            let native = endpoints
                .iter()
                .map(|row| row["native_method"].as_str().unwrap())
                .collect::<std::collections::BTreeSet<_>>();
            let expected = pinned["products"][name]["implicit_endpoints"]
                .as_array()
                .unwrap()
                .iter()
                .map(|method| method.as_str().unwrap())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(native.len(), endpoints.len(), "duplicate native endpoint");
            assert_eq!(
                native, expected,
                "native inventory differs from pinned client"
            );
            assert_eq!(
                endpoints.len() as u64,
                product["implicit_endpoint_count"].as_u64().unwrap()
            );
            for method in product["unified_methods"].as_array().unwrap() {
                let key = method["method"].as_str().unwrap();
                assert_eq!(method["ccxt_has"], pinned["products"][name]["has"][key]);
                for native in method["native_dependencies"].as_array().unwrap() {
                    assert!(endpoints.iter().any(|row| row["native_method"] == *native));
                }
                if key.ends_with("Ws") {
                    assert_eq!(method["coverage"], "excluded_streaming");
                }
            }
        }
    }
    #[test]
    fn native_response_preserves_cursor_headers_and_exact_decimals() {
        let mut response = gofin_exchange::coinbase::CoinbaseResponse::new(
            200,
            br#"[{"balance":"1.000000000000000001"}]"#.to_vec(),
        );
        assert_eq!(
            response_value(&response).unwrap()[0]["balance"],
            "1.000000000000000001"
        );
        response.pagination.after = Some("next-page".into());
        let value = response_value(&response).unwrap();
        assert_eq!(value["data"][0]["balance"], "1.000000000000000001");
        assert_eq!(value["pagination"]["after"], "next-page");
        assert!(value["pagination"]["before"].is_null());
    }
    #[test]
    fn native_matrix_operations_are_compiled_and_get_paths_match() {
        let matrix: Value = serde_json::from_str(MATRIX_JSON).unwrap();
        let definitions = crate::contracts::definitions();
        for product in matrix["products"].as_object().unwrap().values() {
            for row in product["implicit_endpoints"].as_array().unwrap() {
                if let Some(operation) = row["fin_operation"].as_str() {
                    assert!(
                        definitions.iter().any(|d| d.name == operation),
                        "{operation}"
                    );
                    if let Some(d) = descriptor(operation) {
                        assert_eq!(row["native_path"], d.path);
                    }
                }
            }
        }
    }
    #[test]
    fn matrix_links_acronym_methods_to_pinned_native_contracts() {
        let matrix: Value = serde_json::from_str(MATRIX_JSON).unwrap();
        let expected = [
            (
                "coinbase",
                "v3_public_get_brokerage_market_products_product_id_candles",
            ),
            ("coinbaseexchange", "public_get_products_id_candles"),
            (
                "coinbaseinternational",
                "v1_public_get_instruments_instrument_candles",
            ),
        ];
        for (provider, native_method) in expected {
            let product = &matrix["products"][provider];
            let method = product["unified_methods"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["method"] == "fetchOHLCV")
                .unwrap();
            assert!(
                method["native_dependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|dependency| dependency == native_method),
                "{provider}"
            );
            assert!(
                product["implicit_endpoints"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|endpoint| endpoint["native_method"] == native_method)
            );
        }
    }
    #[test]
    fn all_native_reads_have_closed_valid_schemas_and_get_transport() {
        let mut names = std::collections::BTreeSet::new();
        for d in READS {
            assert!(names.insert(d.operation));
            let validator = jsonschema::validator_for(&schema(d)).unwrap();
            let mut input = json!({"account":"synthetic"});
            for key in d.parameters {
                input[key] = "synthetic-id".into();
            }
            if d.product == "coinbase" && d.path.ends_with("/candles") {
                input["query"] =
                    json!({"start":"1700000000","end":"1700000600","granularity":"ONE_MINUTE"});
            }
            if d.path.ends_with("product_book") {
                input["query"] = json!({"product_id":"BTC-USDC"});
            }
            assert!(
                validator.is_valid(&input),
                "{} {:?}",
                d.operation,
                validator
                    .iter_errors(&input)
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
            );
            let route = endpoint(d, &input);
            assert_eq!(route.method(), "GET");
            assert!(!route.mutating());
            assert!(route.path().unwrap().starts_with('/'));
            input["endpoint"] = "https://attacker.invalid".into();
            assert!(!validator.is_valid(&input));
        }
    }
    #[test]
    fn native_path_parameters_are_encoded_and_query_is_closed() {
        let d = READS
            .iter()
            .find(|d| d.path == "/v2/accounts/{account_id}/transactions")
            .unwrap();
        let input = json!({"account":"synthetic","account_id":"a/b?x=1","query":{"limit":2,"starting_after":"z&injected=1"}});
        assert!(
            jsonschema::validator_for(&schema(d))
                .unwrap()
                .is_valid(&input)
        );
        let path = endpoint(d, &input).path().unwrap();
        assert!(path.contains("a%2Fb%3Fx%3D1"));
        assert!(path.contains("z%26injected%3D1"));
        let mut invalid = input;
        invalid["query"]["header"] = "override".into();
        assert!(
            !jsonschema::validator_for(&schema(d))
                .unwrap()
                .is_valid(&invalid)
        );
    }
}
