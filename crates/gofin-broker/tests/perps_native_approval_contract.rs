#![cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;
use gofin_domain::{
    Amount, AssetId, FinancialIntent, PreparedOperation, Quantity, ResourceAmount, ResourceId,
};
use serde_json::json;

#[test]
fn native_perps_request_reviews_fit() {
    let asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
        decimals: 6,
    };
    for op in [
        "jupiter.perps.increase_market",
        "jupiter.perps.limit.create",
        "jupiter.perps.tpsl.create",
        "jupiter.perps.limit.cancel",
    ] {
        let mut p = PreparedOperation {
            operation: op.into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"market":"SOL", "side":1, "size_usd_atoms":"100000000", "request":"11111111111111111111111111111111111111111111", "funding_mint":asset.reference, "funding_decimals":6, "maximum_protocol_fee_usd_atoms":"500000", "maximum_account_funding_lamports":"10000"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        if op == "jupiter.perps.increase_market" {
            p.binding["position_and_borrow_fee_ceiling_usd_atoms"] = json!("90000");
            p.binding["collateral_swap_fee_ceiling_usd_atoms"] = json!("10000");
            p.binding["collateral_mint"] = json!(asset.reference);
            p.binding["collateral_decimals"] = json!(6);
            p.binding["maximum_protocol_fee_atoms"] = json!("500000");
            p.binding["minimum_net_collateral_atoms"] = json!("900000");
            p.intent.debits.push(ResourceAmount {
                resource: ResourceId("owner".into()),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: Quantity::from_u64(1000000),
                },
            });
        }
        if op.contains("create") {
            p.binding["trigger_price_usd_atoms"] = json!("120000000");
            p.binding["trigger_above_threshold"] = json!(true);
        }
        let reason = native_approval::assert_prepared_renders(&p);
        for term in [
            "SOL",
            "Long",
            "Size USD 100",
            "USD fees ≤total 0.5",
            "Funding≤0.00001 SOL",
        ] {
            assert!(reason.contains(term), "{op} omitted {term}: {reason}");
        }
        if op == "jupiter.perps.limit.cancel" {
            assert!(reason.contains("Req 111111…1111"));
        }
        if op == "jupiter.perps.increase_market" {
            for term in [
                "1 USDC",
                "pos+borrow 0.09",
                "swap 0.01",
                "Token fee≤0.5 USDC",
                "Collateral≥0.9 USDC",
            ] {
                assert!(reason.contains(term), "{op} omitted {term}: {reason}");
            }
        }
        if op.contains("create") {
            assert!(
                reason.contains("Trigger USD 120") && reason.contains("Trigger ≥"),
                "{op}: {reason}"
            );
        }
    }
}
