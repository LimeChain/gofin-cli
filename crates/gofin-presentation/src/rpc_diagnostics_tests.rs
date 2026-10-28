use super::*;

fn doctor() -> Value {
    json!({"configurationVersion":2,"financialReadiness":"requires-live-probes","rpc_health":[{
        "chain_id":42161,"read_timeout_ms":null,"endpoints":[
            {"endpoint_id":"rpc-1","identity_verified":true,"status":"cooling_down","retry_after_ms":12000,"last_error_code":"RPC_RATE_LIMITED","unsupported_methods":[]},
            {"endpoint_id":"rpc-2","identity_verified":true,"status":"healthy","retry_after_ms":0,"last_error_code":null,"unsupported_methods":[]}
        ],"last_read":{"method":"eth_call","selected_endpoint":"rpc-2","attempts":[{"endpoint_id":"rpc-1","reason":"rate_limited"},{"endpoint_id":"rpc-2","reason":"ok"}],"outcome":"ok"}
    }]})
}

#[test]
fn all_missing_receipts_are_reported_without_claiming_a_successful_provider() {
    let mut value = doctor();
    value["rpc_health"][0]["last_read"] = json!({"method":"eth_getTransactionReceipt","outcome":"missing","selected_endpoint":null,"attempts":[{"endpoint_id":"rpc-1","reason":"missing"},{"endpoint_id":"rpc-2","reason":"missing"}]});
    let text = render_human("doctor", &value, false);
    assert!(text.contains("no RPC supplied the requested eth_getTransactionReceipt evidence"));
    assert!(!text.contains("answered"));
}
