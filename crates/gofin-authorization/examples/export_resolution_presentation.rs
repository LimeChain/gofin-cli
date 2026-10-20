//! Synthetic Rust-to-Swift review fixture. No credentials, service, or approval.
use gofin_domain::{
    AccountingLocation, Amount, AssetId, AuthorizationChallenge, CostBound, CostTreatment,
    Quantity, ResourceAmount, ResourceId, binding_hash,
    operation_recovery::{OperationResolutionInput, OperationResolutionOutcome},
};
use serde_json::json;

fn main() {
    let wallet = ResourceId("ethereum:1:0x1111111111111111111111111111111111111111".into());
    let recipient = ResourceId("coinbase:portfolio:22222222-3333-4444-8555-666666666666".into());
    let usdc = AssetId {
        network: "ethereum:1".into(),
        reference: "0x3333333333333333333333333333333333333333".into(),
        decimals: 6,
    };
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let venue_usdc = AssetId {
        network: "coinbase".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let debit = ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: usdc.clone(),
            atoms: Quantity::from_decimal("500", 6).unwrap(),
        },
    };
    let cost = CostBound {
        location: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: Quantity::from_decimal("0.000012345678901234", 18).unwrap(),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let receipt = ResourceAmount {
        resource: recipient.clone(),
        amount: Amount {
            asset: venue_usdc.clone(),
            atoms: Quantity::from_decimal("500", 36).unwrap(),
        },
    };
    let input = OperationResolutionInput { outcome:OperationResolutionOutcome::Succeeded, reason:"Human checked the exchange account and accepts the reported credit despite incomplete automatic destination evidence.".into(), actual_debits:vec![debit.clone()], actual_costs:vec![cost.clone()], reported_receipts:vec![receipt] };
    // These are the real tuple encodings used by resource_asset_key/cost_key.
    // They intentionally include JSON punctuation; they are not resource IDs.
    let cash_key = serde_json::to_string(&(wallet.0.clone(), usdc.clone())).unwrap();
    let gas_key = serde_json::to_string(&(wallet.0.clone(), native.clone())).unwrap();
    let cost_key = serde_json::to_string(&(
        cost.location.clone(),
        native.clone(),
        cost.treatment.clone(),
    ))
    .unwrap();
    let position_key = serde_json::to_string(&(
        AccountingLocation::Position {
            resource: recipient.clone(),
            position: "synthetic-position".into(),
            position_kind: gofin_domain::PositionKind::DerivativeLong,
        },
        venue_usdc.clone(),
        CostTreatment::IncludedInDebit,
    ))
    .unwrap();
    let review = json!({
        "version":1,"operation_id":"77777777-8888-4999-aaaa-bbbbbbbbbbbb","operation":"coinbase.deposit",
        "original_request_id":"cccccccc-dddd-4eee-8fff-111111111111","fingerprint":"a".repeat(64),
        "submission":{"kind":"evm","reference":format!("0x{}","2".repeat(64)),"data":{"synthetic":true}},
        "record_hash":"b".repeat(64),"reservations_hash":"c".repeat(64),"plan_hash":"d".repeat(64),"attestation":input,
        "resources":[wallet,recipient],
        "released_reservations":[
            {"resource":cash_key,"units":"500000000","fee_units":"0"},
            {"resource":gas_key,"units":"900000000000000","fee_units":"0"},
            {"resource":cost_key,"units":"0","fee_units":"0"},
            {"resource":format!("lock:{}",wallet.0),"units":"0","fee_units":"0"},
            {"resource":format!("lock:{}",recipient.0),"units":"0","fee_units":"0"}],
        "released_cash":[{"resource":wallet,"asset":usdc,"amount_atoms":"500000000"},{"resource":wallet,"asset":native,"amount_atoms":"900000000000000"}],
        "remaining_reservations":[{"resource":"lock:ethereum:1:0x4444444444444444444444444444444444444444","units":"0","fee_units":"0"}],
        "impact":{"historical_cap_breach":false,"future_authority_increased":false,"charged_debits":[debit],"charged_costs":[cost],"allocation_charges_retained":true,"plan_usage_retained":true,"additional_allocation_usd_micros":"0","receipt_provenance":"human_attested","plan_linked":true},
        "warnings":["Original submission may still act; human attestation is not external verification."]
    });
    let binding = json!({"version":1,"action":"operation_resolution","request_id":"eeeeeeee-ffff-4111-8222-333333333333","review_hash":binding_hash(&review).unwrap(),"review":review});
    let now = chrono::Utc::now();
    gofin_authorization::validate_challenge(
        &AuthorizationChallenge {
            purpose: "operation_resolution".into(),
            operation_id: review["operation_id"].as_str().unwrap().into(),
            nonce: "f".repeat(64),
            binding_hash: binding_hash(&binding).unwrap(),
            binding: binding.clone(),
            summary: "Synthetic presentation compatibility test".into(),
            expires_at: (now + chrono::Duration::seconds(60)).to_rfc3339(),
        },
        &now.to_rfc3339(),
    )
    .unwrap();
    println!(
        "{}",
        json!({"binding":binding,"position_cost_key":position_key,"expected_quantities":["500","0.000012345678901234","0.0009"],"expected_resources":[wallet,recipient],"expected_asset_references":[usdc.reference,native.reference,venue_usdc.reference]})
    );
}
