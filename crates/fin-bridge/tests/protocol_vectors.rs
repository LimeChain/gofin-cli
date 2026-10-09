use fin_bridge::relay::order::hash_order;
use serde_json::Value;

#[test]
fn maintained_sdk_v1_order_vectors_match() {
    let vectors: Value = serde_json::from_str(include_str!("fixtures/order-vectors.json")).unwrap();
    assert_eq!(vectors["sdk_version"], "0.0.146");
    assert_eq!(
        vectors["sdk_git_head"],
        "8e7d794475836ae77e7f4cf3fc788f02ed079150"
    );
    for vector in vectors["vectors"].as_array().unwrap() {
        let order = &vector["order"];
        let expected = vector["order_id"].as_str().unwrap();
        assert_eq!(
            format!("{:#x}", hash_order(order).unwrap()),
            expected,
            "{}",
            vector["name"]
        );
    }
}

#[test]
fn changing_any_hashed_order_field_changes_the_id() {
    let vectors: Value = serde_json::from_str(include_str!("fixtures/order-vectors.json")).unwrap();
    let original = &vectors["vectors"][0]["order"];
    let original_id = hash_order(original).unwrap();
    let mut changed = original.clone();
    changed["inputs"][0]["payment"]["amount"] = "3000000000000001".into();
    assert_ne!(hash_order(&changed).unwrap(), original_id);
}
