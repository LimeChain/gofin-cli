//! Compatibility for the audited Fin -> GoFin presentation-only catalog edits.
//!
//! These exact aliases preserve already approved plan hashes. A general text
//! replacement could collapse material provider names, schema enums/defaults or
//! authority changes. Match the operation, JSON pointer AND complete renamed
//! string instead; every other byte of the registered contract remains bound.

use crate::{Definition, Result, binding_hash};
use serde_json::{Value, json};

/// Hash an exact plan's operation contract, retaining pre-GoFin approval identity.
/// Only the finite catalog presentation aliases below receive legacy spelling.
/// Protected connection identities are bound separately by the engine.
pub fn exact_plan_definition_hash(definition: &Definition) -> Result<String> {
    let mut binding = json!({
        "operation": definition.name,
        "kind": definition.kind,
        "execution_actor": definition.execution_actor,
        "input_schema": definition.input_schema,
        "semantics": definition.semantics,
    });
    for &(operation, pointer, legacy) in PRESENTATION_ALIASES {
        if definition.name != operation {
            continue;
        }
        let Some(value) = binding.pointer_mut(pointer) else {
            continue;
        };
        let renamed = renamed_presentation(legacy);
        if value.as_str() == Some(renamed.as_str()) {
            *value = Value::String(legacy.to_owned());
        }
    }
    binding_hash(&binding)
}

fn renamed_presentation(legacy: &str) -> String {
    let renamed = legacy.replace("Fin", "GoFin");
    match renamed.strip_prefix("fin ") {
        Some(command) => format!("gofin {command}"),
        None => renamed,
    }
}

// Audited against the pre-rename financial catalog. This is an exact migration
// allowlist, not a list of fields in which future text changes may be ignored.
const PRESENTATION_ALIASES: &[(&str, &str, &str)] = &[
    (
        "binance.deposit",
        "/semantics/sourceCoverage",
        "Configured Fin EVM chains and Solana mainnet-beta wallets. Solana token transfers use an explicit mint or the pinned mainnet USDC mint; other assets, memo/tag routes and non-unit denominations are unavailable.",
    ),
    (
        "binance.deposit-addresses",
        "/semantics/next_check",
        "fin read binance.deposit-addresses --input JSON --json",
    ),
    (
        "binance.futures.order-cancel",
        "/semantics/target",
        "entire original durable Submission plus its Fin operation_id",
    ),
    (
        "binance.spot.order-cancel",
        "/semantics/target",
        "entire original durable Submission plus its Fin operation_id",
    ),
    (
        "binance.transfer",
        "/semantics/authority",
        "For main→child or legacy main-authorized child→main, select the main account. For source-child→parent only, select the child as authority; Fin uses subToMaster and sends no clientTranId or recipient parameter.",
    ),
    (
        "binance.transfer",
        "/semantics/subToMasterReceipt",
        "Fin persists Binance txnId against the immutable local submission before returning. If the response is lost before the receipt is persisted, the result remains unknown and history is not inferred from amount or time.",
    ),
    (
        "binance.whitelist",
        "/semantics/next_check",
        "fin read binance.whitelist --input JSON --json",
    ),
    (
        "bridge.execute",
        "/input_schema/$defs/BridgeBounds/description",
        "All caps are explicit and asset-denominated. USD caps use integer microdollars;\na caller must supply independent Fin pricing evidence before policy can enforce them.",
    ),
    (
        "bridge.execute",
        "/semantics/command_line_example",
        "fin plans preview --input PLAN_JSON --json",
    ),
    (
        "bridge.quotes",
        "/input_schema/$defs/BridgeBounds/description",
        "All caps are explicit and asset-denominated. USD caps use integer microdollars;\na caller must supply independent Fin pricing evidence before policy can enforce them.",
    ),
    (
        "bridge.quotes",
        "/semantics/command_line_example",
        "fin read bridge.quotes --input JSON --json",
    ),
    (
        "bridge.quotes",
        "/semantics/provider_usd_fields",
        "ignored; fee USD values are independently priced by Fin",
    ),
    (
        "bridge.transfer",
        "/input_schema/$defs/BridgeBounds/description",
        "All caps are explicit and asset-denominated. USD caps use integer microdollars;\na caller must supply independent Fin pricing evidence before policy can enforce them.",
    ),
    (
        "bridge.transfer",
        "/semantics/command_line_example",
        "fin preview bridge.transfer --input '{\"transferId\":\"TRANSFER_ID\",\"stepId\":\"STEP_ID\",\"stepKind\":\"deposit\",\"bounds\":{...}}'",
    ),
    (
        "hyperliquid.account.invalidate_nonce",
        "/input_schema/description",
        "Invalidate the exact nonce used by one unresolved Hyperliquid class transfer.\nThe nonce itself is loaded only from Fin's durable original submission.",
    ),
    (
        "hyperliquid.account.invalidate_nonce",
        "/semantics/nonce",
        "loaded from the immutable original Fin submission; caller cannot supply a nonce",
    ),
    (
        "hyperliquid.orders.edit",
        "/input_schema/properties/parent_operation_id/description",
        "Durable Fin order operation whose submission created this native order.\nRequired when linking this replacement to an existing Fin reservation.",
    ),
    (
        "hyperliquid.orders.edit_batch",
        "/input_schema/$defs/EditOrderItem/properties/parent_operation_id/description",
        "Durable Fin order operation whose submission created this native order.\nRequired when linking this replacement to an existing Fin reservation.",
    ),
    (
        "hyperliquid.orders.twap",
        "/input_schema/properties/estimated_notional_usdc/description",
        "Estimated gross notional used for Fin's reservation and review. It is\nnot a venue-enforced lifetime quote-price ceiling.",
    ),
    (
        "hyperliquid.orders.twap",
        "/input_schema/properties/estimated_realized_profit_usdc/description",
        "Estimated positive realized PnL allowance used by Fin's settlement\nexpectation. It is not a venue-enforced cap.",
    ),
    (
        "hyperliquid.orders.twap",
        "/semantics/estimates",
        "Fin reconciliation limits only; none are guaranteed by the native venue schedule",
    ),
    (
        "jupiter.limit.cancel",
        "/input_schema/properties/original_operation_id/description",
        "Required only when cancelling an order created by Fin. Omit for an externally created wallet-owned order; Fin then verifies the on-chain owner and uses a fresh standalone approval.",
    ),
    (
        "jupiter.limit.cancel",
        "/input_schema/properties/original_submission/description",
        "Optional exact echo of Fin's saved submission when original_operation_id is supplied. Omit for externally created orders; a named-plan recovery step supplies it from the earlier create step.",
    ),
    (
        "jupiter.perps.increase_market",
        "/input_schema/properties/funding_token/description",
        "Wallet debit token. The Perps builder may internally convert any supported input token to the market's required collateral; Fin enforces a price-derived minimum conversion output and collateral-token fee cap.",
    ),
    (
        "jupiter.perps.limit.cancel",
        "/input_schema/properties/original_operation_id/description",
        "Exact pending jupiter.perps.limit.create operation ID from Fin status; null only for a named-plan recovery placeholder.",
    ),
    (
        "jupiter.perps.limit.cancel",
        "/input_schema/properties/original_submission/description",
        "Optional exact submission echo. Fin normally loads the immutable submission from the local operation journal; named-plan recovery fills this field itself.",
    ),
    (
        "jupiter.perps.limit.update",
        "/input_schema/properties/original_operation_id/description",
        "Exact pending limit.create operation ID from Fin status; null for a named-plan recovery placeholder.",
    ),
    (
        "jupiter.perps.tpsl.create",
        "/input_schema/properties/protection_parent_operation_id/description",
        "Exact unresolved Fin operation ID of the opposite TP/SL request to pair on this position.",
    ),
    (
        "swap.execute",
        "/semantics/plan_permission/quote_selection",
        "The agent nominates one allowed configured DEX and its fresh quote candidate at execution; Fin checks that candidate against the pinned deployment and bounds.",
    ),
    (
        "wallet.transfer",
        "/input_schema/anyOf/1/properties/max_account_funding_sol/description",
        "Required when Fin must create the recipient associated token account; this funds the recipient's refundable rent.",
    ),
    (
        "wallet.wrap",
        "/input_schema/anyOf/1/properties/max_account_funding_sol/description",
        "Required only when Fin must create the wallet's wrapped-SOL associated account; refundable rent remains wallet-owned.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionActor, OperationKind};

    fn definition() -> Definition {
        Definition {
            name: "bridge.quotes".into(),
            description: "Synthetic contract".into(),
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentReadable,
            input_schema: json!({"type":"object", "properties": {
                "provider":{"enum":["Fin"], "default":"Fin", "const":"Fin"},
                "amount":{"type":"number", "minimum":1, "maximum":10}
            }}),
            output_schema: json!({}),
            error_schema: json!({}),
            examples: vec![],
            coverage: "synthetic".into(),
            completion: "read only".into(),
            semantics: json!({"authority":{"maximum":10}, "provider":"Fin"}),
        }
    }

    // Independent pre-rename algorithm: do not call the compatibility helper.
    fn legacy_hash(definition: &Definition) -> String {
        binding_hash(&json!({
            "operation": definition.name,
            "kind": definition.kind,
            "execution_actor": definition.execution_actor,
            "input_schema": definition.input_schema,
            "semantics": definition.semantics,
        }))
        .unwrap()
    }

    fn insert_pointer(value: &mut Value, pointer: &str, text: &str) {
        let segments: Vec<_> = pointer.trim_start_matches('/').split('/').collect();
        let mut current = value;
        for (index, segment) in segments.iter().enumerate() {
            if current.is_null() {
                *current = if segment.parse::<usize>().is_ok() {
                    json!([])
                } else {
                    json!({})
                };
            }
            current = if let Value::Array(array) = current {
                let index: usize = segment.parse().unwrap();
                array.resize(array.len().max(index + 1), Value::Null);
                &mut array[index]
            } else {
                current
                    .as_object_mut()
                    .unwrap()
                    .entry(*segment)
                    .or_insert(Value::Null)
            };
            if index == segments.len() - 1 {
                *current = json!(text);
            }
        }
    }

    #[test]
    fn every_audited_presentation_alias_preserves_the_preexisting_hash() {
        for &(operation, pointer, legacy) in PRESENTATION_ALIASES {
            let mut old = serde_json::to_value(definition()).unwrap();
            old["name"] = json!(operation);
            insert_pointer(&mut old, pointer, legacy);
            let old: Definition = serde_json::from_value(old.clone()).unwrap();
            let hash = legacy_hash(&old);
            assert_eq!(exact_plan_definition_hash(&old).unwrap(), hash);
            let mut renamed = serde_json::to_value(&old).unwrap();
            insert_pointer(&mut renamed, pointer, &renamed_presentation(legacy));
            let renamed: Definition = serde_json::from_value(renamed).unwrap();
            assert_ne!(legacy_hash(&renamed), hash, "{operation}{pointer}");
            assert_eq!(
                exact_plan_definition_hash(&renamed).unwrap(),
                hash,
                "{operation}{pointer}"
            );
        }
    }

    #[test]
    fn brand_words_in_material_values_and_other_contract_changes_still_change_hash() {
        let old = definition();
        let hash = legacy_hash(&old);
        let mut mutations = Vec::new();
        for pointer in [
            "/input_schema/properties/provider/enum/0",
            "/input_schema/properties/provider/default",
            "/input_schema/properties/provider/const",
            "/semantics/provider",
        ] {
            let mut value = serde_json::to_value(&old).unwrap();
            *value.pointer_mut(pointer).unwrap() = json!("GoFin");
            mutations.push(serde_json::from_value(value).unwrap());
        }
        for pointer in [
            "/input_schema/properties/amount/minimum",
            "/input_schema/properties/amount/maximum",
            "/semantics/authority/maximum",
        ] {
            let mut value = serde_json::to_value(&old).unwrap();
            *value.pointer_mut(pointer).unwrap() = json!(11);
            mutations.push(serde_json::from_value(value).unwrap());
        }
        let mut changed = old.clone();
        changed.kind = OperationKind::Write;
        mutations.push(changed);
        let mut changed = old.clone();
        changed.execution_actor = ExecutionActor::HumanInteractive;
        mutations.push(changed);
        for changed in mutations {
            assert_ne!(exact_plan_definition_hash(&changed).unwrap(), hash);
        }
    }

    #[test]
    fn only_exact_audited_paths_and_complete_strings_are_aliased() {
        let (operation, pointer, legacy) = PRESENTATION_ALIASES[0];
        let mut old = serde_json::to_value(definition()).unwrap();
        old["name"] = json!(operation);
        insert_pointer(&mut old, pointer, legacy);
        let old: Definition = serde_json::from_value(old).unwrap();
        let hash = legacy_hash(&old);
        let mut changed = serde_json::to_value(&old).unwrap();
        insert_pointer(
            &mut changed,
            pointer,
            &format!("{} Authority limit: 100.", renamed_presentation(legacy)),
        );
        let changed: Definition = serde_json::from_value(changed).unwrap();
        assert_ne!(exact_plan_definition_hash(&changed).unwrap(), hash);

        // Even a complete presentation alias remains material in an enum or
        // inside an authority object, rather than its audited prose location.
        for pointer in [
            "/input_schema/properties/provider/enum/0",
            "/semantics/authority/provider",
        ] {
            let mut old = serde_json::to_value(definition()).unwrap();
            old["name"] = json!(operation);
            insert_pointer(&mut old, pointer, legacy);
            let mut renamed = old.clone();
            insert_pointer(&mut renamed, pointer, &renamed_presentation(legacy));
            let old: Definition = serde_json::from_value(old).unwrap();
            let renamed: Definition = serde_json::from_value(renamed).unwrap();
            assert_ne!(
                exact_plan_definition_hash(&renamed).unwrap(),
                legacy_hash(&old)
            );
        }
    }
}
