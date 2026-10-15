//! Offline input contracts matching Binance's exact decimal parser.
use schemars::{Schema, SchemaGenerator, json_schema};

pub(super) fn unsigned(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type":"string",
        "pattern":"^(0|[1-9][0-9]*)(\\.[0-9]{1,36})?$",
        "description":"Exact unsigned decimal string with a whole-number digit, for example 0.00415, not .00415. No signs, exponents or leading zeros; at most 36 fractional digits."
    })
}

pub(super) fn positive(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = unsigned(generator);
    schema.insert("not".into(), serde_json::json!({"pattern":"^0(\\.0+)?$"}));
    schema
}

pub(super) fn optional_unsigned(generator: &mut SchemaGenerator) -> Schema {
    let decimal = unsigned(generator);
    json_schema!({"anyOf":[decimal,{"type":"null"}]})
}

pub(super) fn optional_positive(generator: &mut SchemaGenerator) -> Schema {
    let decimal = positive(generator);
    json_schema!({"anyOf":[decimal,{"type":"null"}]})
}

pub(super) fn positive_or_all(generator: &mut SchemaGenerator) -> Schema {
    let decimal = positive(generator);
    json_schema!({"anyOf":[decimal,{"const":"all"}]})
}

pub(super) fn fee_map(generator: &mut SchemaGenerator) -> Schema {
    let decimal = positive(generator);
    json_schema!({"type":"object","additionalProperties":decimal,"minProperties":1})
}
