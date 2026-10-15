//! Regenerate the offline catalog from the compiled registry without configuration or credentials.
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&gofin_broker::operation_definitions())
            .expect("financial definitions serialize")
    );
}
