//! Regenerate offline financial discovery from the protected runtime registry.
fn sort_objects(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let sorted: std::collections::BTreeMap<_, _> = fields
                .into_iter()
                .map(|(key, value)| (key, sort_objects(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_objects).collect())
        }
        value => value,
    }
}
fn main() {
    let definitions = serde_json::Value::Array(
        gofin_broker::operation_definitions()
            .into_iter()
            .map(|definition| {
                let mut value = serde_json::to_value(definition).expect("definitions serialize");
                // Keep the public definition field order; sort nested schemas/maps recursively.
                if let serde_json::Value::Object(fields) = &mut value {
                    for nested in fields.values_mut() {
                        *nested = sort_objects(nested.take());
                    }
                }
                value
            })
            .collect(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&definitions).expect("catalog serializes")
    );
}
