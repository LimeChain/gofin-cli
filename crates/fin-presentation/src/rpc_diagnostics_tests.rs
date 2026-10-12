use super::*;

fn doctor() -> Value {
    json!({"configurationVersion":2,"financialReadiness":"requires-live-probes","rpc_health":[{
        "chain_id":42161,"read_timeout_ms":10000,"endpoints":[
            {"endpoint_id":"rpc-1","identity_verified":true,"status":"cooling_down","retry_after_ms":12000,"last_error_code":"RPC_RATE_LIMITED","unsupported_methods":[]},
            {"endpoint_id":"rpc-2","identity_verified":true,"status":"healthy","retry_after_ms":0,"last_error_code":null,"unsupported_methods":[]}
        ],"last_read":{"method":"eth_call","selected_endpoint":"rpc-2","attempts":[{"endpoint_id":"rpc-1","reason":"rate_limited"},{"endpoint_id":"rpc-2","reason":"ok"}],"outcome":"ok"}
    }]})
}

fn ui(machine: bool, verbose: bool, tty: bool) -> Ui {
    Ui {
        machine,
        verbose: verbose && !machine,
        terminal: TerminalPolicy::from_environment(tty, tty, false, false),
    }
}

#[test]
fn doctor_shows_observed_failover_briefly_and_keeps_details_in_verbose_and_json() {
    let value = doctor();
    for tty in [false, true] {
        for machine in [false, true] {
            for verbose in [false, true] {
                let mut output = Vec::new();
                ui(machine, verbose, tty)
                    .write_result_to(&value, "doctor", &mut output)
                    .unwrap();
                assert!(!output.contains(&0x1b));
                if machine {
                    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
                } else {
                    let text = String::from_utf8(output).unwrap();
                    if verbose {
                        assert!(text.contains("retry_after_ms: 12000"));
                        assert!(text.contains("last_error_code: RPC_RATE_LIMITED"));
                    } else {
                        assert!(text.contains("Chain 42161: rpc-2 answered eth_call."));
                        assert!(text.contains("rpc-1: rate limited."));
                        assert!(text.contains("observed reads; no new network probes"));
                        assert!(!text.contains("retry_after_ms"));
                        assert!(!text.contains("RPC_RATE_LIMITED"));
                        assert!(text.contains("fin doctor --verbose"));
                    }
                    for private in ["https://", "Authorization", "api_key", "password"] {
                        assert!(!text.contains(private));
                    }
                }
            }
        }
    }
}

#[test]
fn doctor_distinguishes_cancelled_and_failed_reads_from_no_observations() {
    let mut value = doctor();
    value["rpc_health"][0]["last_read"]["outcome"] = json!("cancelled");
    assert!(render_human("doctor", &value, false).contains("eth_call was cancelled"));
    value["rpc_health"][0]["last_read"]["outcome"] = json!("error");
    value["rpc_health"][0]["last_read"]["attempts"][0]["reason"] = json!("historical_unavailable");
    let failed = render_human("doctor", &value, false);
    assert!(failed.contains("eth_call failed"));
    assert!(failed.contains("historical state was unavailable"));
    value["rpc_health"][0]["last_read"] = Value::Null;
    let no_reads = render_human("doctor", &value, false);
    assert!(no_reads.contains("2 RPC endpoints; no reads observed"));
    assert!(!no_reads.contains("answered"));
    value["rpc_health"] = json!([]);
    assert!(render_human("doctor", &value, false).contains("no configured RPC transports"));
}

#[test]
fn all_missing_receipts_are_reported_without_claiming_a_successful_provider() {
    let mut value = doctor();
    value["rpc_health"][0]["last_read"] = json!({"method":"eth_getTransactionReceipt","outcome":"missing","selected_endpoint":null,"attempts":[{"endpoint_id":"rpc-1","reason":"missing"},{"endpoint_id":"rpc-2","reason":"missing"}]});
    let text = render_human("doctor", &value, false);
    assert!(text.contains("no RPC supplied the requested eth_getTransactionReceipt evidence"));
    assert!(!text.contains("answered"));
}

#[test]
fn subsequent_healthy_read_keeps_most_recent_fallback_visible_in_all_output_modes() {
    let mut value = doctor();
    value["rpc_health"][0]["last_failover"] = value["rpc_health"][0]["last_read"].clone();
    value["rpc_health"][0]["last_failover"]["method"] = json!("eth_getTransactionReceipt");
    value["rpc_health"][0]["last_read"] = json!({"method":"eth_getBlockByNumber","selected_endpoint":"rpc-1","attempts":[{"endpoint_id":"rpc-1","reason":"ok"}],"outcome":"ok"});
    for tty in [false, true] {
        for machine in [false, true] {
            for verbose in [false, true] {
                let mut output = Vec::new();
                ui(machine, verbose, tty)
                    .write_result_to(&value, "doctor", &mut output)
                    .unwrap();
                assert!(!output.contains(&0x1b));
                if machine {
                    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
                } else {
                    let text = String::from_utf8(output).unwrap();
                    if verbose {
                        assert!(text.contains("last_failover"));
                        assert!(text.contains("eth_getTransactionReceipt"));
                    } else {
                        assert!(text.contains("rpc-1 answered eth_getBlockByNumber"));
                        assert!(text.contains(
                            "Most recent fallback: rpc-2 answered eth_getTransactionReceipt."
                        ));
                        assert!(text.contains("rpc-1: rate limited."));
                    }
                }
            }
        }
    }
    value["rpc_health"][0]["last_read"] = value["rpc_health"][0]["last_failover"].clone();
    assert!(!render_human("doctor", &value, false).contains("Most recent fallback"));
}
