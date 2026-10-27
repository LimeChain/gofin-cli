//! Synthetic Rust-validated reviews consumed by the native presentation test.
use gofin_domain::approval_review::{ApprovalAction, ApprovalReview};
use serde_json::{Value, json};

fn main() {
    let schema = serde_json::to_value(schemars::schema_for!(ApprovalAction)).unwrap();
    let mut reviews = Vec::new();
    for variant in schema["enum"].as_array().unwrap() {
        let action = variant.as_str().unwrap();
        let leg = json!({"amount":"1.25","asset":{"name":"USDC","identifier":{"network":"ethereum:1","reference":"native","decimals":6}},"account":{"name":"wallet","domain":"chain"}});
        let output = leg.clone();
        let mut input = leg;
        input["asset"]["name"] = json!("ORE");
        input["asset"]["identifier"]["reference"] = json!("synthetic-ore-address");
        let mut review = json!({"version":2,"action":action,"service":"Example","market":"market","input":input,"output":output,"other_details":"fee cap 0.01 USDC"});
        if action == "execute-plan" {
            review["description"] = json!("Plan");
            review["step_count"] = json!(1);
            review["valid_for_seconds"] = json!(60);
        }
        let typed: ApprovalReview = serde_json::from_value(review).unwrap();
        typed.validate().unwrap();
        let purpose = match action {
            "execute-plan" => "financial_plan",
            "authorize-boundaries" => "financial_boundary",
            _ => "financial_write",
        };
        let challenge = gofin_domain::AuthorizationChallenge {
            review: Some(typed),
            purpose: purpose.into(),
            operation_id: "fixture".into(),
            binding: json!({}),
            binding_hash: String::new(),
            summary: String::new(),
            nonce: "fixture".into(),
            expires_at: "fixture".into(),
        };
        let helper = gofin_authorization::helper_challenge(&challenge).unwrap();
        reviews.push(
            serde_json::from_str::<Value>(helper["canonicalBinding"].as_str().unwrap()).unwrap(),
        );
    }
    println!("{}", serde_json::to_string(&Value::Array(reviews)).unwrap());
}
