//! Historical CCTP authority is interpreted without modifying the signed intent.
use super::*;
use gofin_domain::Authorizer;

struct UnusedAuthorizer;
#[async_trait]
impl Authorizer for UnusedAuthorizer {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        panic!("read-only observation must not request approval")
    }
}

#[test]
fn historical_destination_withheld_fee_matches_exact_cap_only() {
    for (actual, cap_kind, expected_attention) in [
        (500u64, "withheld_from_output", "none"),
        (5001, "withheld_from_output", "discrepancy"),
        (500, "additional_debit", "discrepancy"),
    ] {
        let asset = json!({"network":"ethereum:42161","reference":"SYNTHETIC-USDC","decimals":6});
        let resource = "ethereum:42161:SYNTHETIC-wallet";
        let timestamp = now();
        let input =
            json!({"bounds":{"fee_caps":[{"asset":asset,"kind":cap_kind,"maximum":"5000"}]}});
        let preview: PreparedOperation = serde_json::from_value(json!({
            "operation":"bridge.execute","input":input,"binding":{"transfer_id":"synthetic-transfer"},
            "summary":"synthetic historical bridge","checks":[],"warnings":[],"observed_at":timestamp,"expires_at":"2099-01-01T00:00:00Z",
            "intent":{"debits":[],"maximum_costs":[{"location":{"kind":"cash","resource":resource},"amount":{"asset":asset,"atoms":"5000"},"treatment":"included_in_debit"}],
                "expected":[{"kind":"transfer","from":"ethereum:1:SYNTHETIC-wallet","to":resource,"debit_asset":{"network":"ethereum:1","reference":"SYNTHETIC-USDC","decimals":6},"credit_asset":asset,"maximum_debit":"10000000","minimum_credit":"9000000"}],"locks":[]},"operation_version":1
        })).unwrap();
        let action: OperationRecord = serde_json::from_value(json!({"id":"synthetic-action","request_id":"synthetic-request","fingerprint":"synthetic","fingerprint_version":1,"operation":"bridge.execute","input":input,"status":"pending","created_at":timestamp,"updated_at":timestamp,"preview":preview,"approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null})).unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                &action.id,
                &action.request_id,
                &action.fingerprint,
                &action.operation,
                None,
                "pending",
                &to_value(&action).unwrap(),
                &action.created_at,
            )
            .unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(UnusedAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let observation = json!({"transfer_id":"synthetic-transfer","state":"completed","settlement":{"evidence_id":"synthetic-proof","cash_credits":[{"resource":resource,"asset":asset,"amount_atoms":"9999500"}],"costs":[{"location":{"kind":"cash","resource":resource},"asset":asset,"amount_atoms":actual.to_string(),"category":"cctp_protocol_fee","treatment":"deducted_from_credit"}]}});
        let updated = engine
            .apply_compound_destination_observation(action, observation)
            .unwrap();
        let view = engine.operation_view(&updated).unwrap();
        assert_eq!(view["outcome"]["result"], "executed");
        assert_eq!(view["outcome"]["transfer"], "available");
        assert_eq!(
            view["outcome"]["attention"]["kind"], expected_attention,
            "{view}"
        );
    }
}
