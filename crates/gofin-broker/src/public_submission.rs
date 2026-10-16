use serde_json::{Map, Value};

/// Remove signed EVM calldata and Solana wire bytes from broker responses while preserving the
/// durable submission identity and all other reconciliation data. The engine
/// and store keep the original submission so adapters can reconcile receipts.
pub(super) fn redact_public_payload(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.remove("signed_transaction_base64").is_some() {
                object.insert("signed_transaction_withheld".into(), Value::Bool(true));
            }
            redact_submission(object);
            for child in object.values_mut() {
                redact_public_payload(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact_public_payload(child);
            }
        }
        _ => {}
    }
}

fn redact_submission(object: &mut Map<String, Value>) {
    if object.get("kind").and_then(Value::as_str) != Some("ethereum") {
        return;
    }
    let Some(data) = object.get_mut("data").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(transaction) = data.get_mut("transaction").and_then(Value::as_object_mut) else {
        return;
    };
    if transaction.remove("calldata").is_some() {
        data.insert("calldata_withheld".into(), Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recursively_withholds_solana_wire_and_preserves_private_source() {
        let original = json!({"raw":{"signed_transaction_base64":"private-wire"},"history":[{"evidence":{"submission":{"kind":"solana.transaction","reference":"solana:message:hash","data":{"schema_version":2,"signed_transaction_base64":"private-wire","message_sha256":"hash","wallet_signature":"signature"}}}}]});
        let mut public = original.clone();
        redact_public_payload(&mut public);
        let data = &public["history"][0]["evidence"]["submission"]["data"];
        assert!(data.get("signed_transaction_base64").is_none());
        assert_eq!(data["signed_transaction_withheld"], true);
        assert_eq!(data["wallet_signature"], "signature");
        assert!(public["raw"].get("signed_transaction_base64").is_none());
        assert_eq!(
            original["history"][0]["evidence"]["submission"]["data"]["signed_transaction_base64"],
            "private-wire"
        );
    }

    #[test]
    fn recursively_withholds_nested_ethereum_submission_calldata_without_mutating_source() {
        let original = json!({
            "status": "failed",
            "attempt": {
                "submission": {
                    "kind": "ethereum",
                    "reference": "0xabc",
                    "data": {
                        "transaction": {
                            "chain_id": 1,
                            "calldata": "0xdeadbeef",
                            "nonce": 8
                        },
                        "metadata": {"operation": "uniswap.swap"}
                    }
                }
            },
            "history": [{
                "kind": "ethereum",
                "reference": "0xdef",
                "data": {"transaction": {"calldata": "0xfeed"}}
            }],
            "proof": {
                "verified": true,
                "evidence": {
                    "origin_submission": {
                        "kind": "ethereum",
                        "reference": "0xghi",
                        "data": {"transaction": {"calldata": "0xpermit-signature"}}
                    }
                }
            }
        });
        let mut public = original.clone();
        redact_public_payload(&mut public);

        assert_eq!(
            public["attempt"]["submission"]["data"]["calldata_withheld"],
            true
        );
        assert!(
            public["attempt"]["submission"]["data"]["transaction"]
                .get("calldata")
                .is_none()
        );
        assert_eq!(
            public["attempt"]["submission"]["data"]["transaction"]["nonce"],
            8
        );
        assert_eq!(public["history"][0]["data"]["calldata_withheld"], true);
        assert!(
            public["history"][0]["data"]["transaction"]
                .get("calldata")
                .is_none()
        );
        assert_eq!(
            public["proof"]["evidence"]["origin_submission"]["data"]["calldata_withheld"],
            true
        );
        assert!(
            public["proof"]["evidence"]["origin_submission"]["data"]["transaction"]
                .get("calldata")
                .is_none()
        );
        assert_eq!(
            original["attempt"]["submission"]["data"]["transaction"]["calldata"],
            "0xdeadbeef"
        );
    }

    #[test]
    fn previews_non_ethereum_submissions_and_unrecognized_objects_remain_unchanged() {
        let mut payload = json!({
            "preview": {"binding": {"transaction": {"calldata": "0x0000"}}},
            "submissions": [
                {"kind": "binance", "reference": "client-1", "data": {"calldata": "fixture"}},
                {"kind": "ethereum", "reference": "0x1", "data": {"transaction": {"input": "0x1234"}}},
                {"kind": "ethereum", "reference": "0x2", "data": {"transaction": {"calldata": null}}}
            ]
        });
        let original = payload.clone();
        redact_public_payload(&mut payload);

        assert_eq!(
            payload["preview"]["binding"]["transaction"]["calldata"],
            "0x0000"
        );
        assert_eq!(payload["submissions"][0], original["submissions"][0]);
        assert_eq!(payload["submissions"][1], original["submissions"][1]);
        assert_eq!(payload["submissions"][2]["data"]["calldata_withheld"], true);
        assert!(
            payload["submissions"][2]["data"]["transaction"]
                .get("calldata")
                .is_none()
        );
    }
}
