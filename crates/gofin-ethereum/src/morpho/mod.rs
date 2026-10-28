//! Morpho V2 vaults and Merkl rewards. Protocol data is not signing authority.
pub mod api;
pub mod permissions;
pub mod profile;
pub mod reads;
pub mod rewards;
pub mod snapshot;
mod valuation;
pub mod vault;

use super::*;

pub fn is_read(operation: &str) -> bool {
    matches!(
        operation,
        "morpho.vaults" | "morpho.vault" | "morpho.positions" | "morpho.rewards"
    )
}

pub fn definitions() -> Vec<Definition> {
    [
        ("morpho.vaults", "Discover listed Morpho V2 vaults by network and underlying asset."),
        ("morpho.vault", "Inspect a Morpho V2 vault, allocations, fees, liquidity and write compatibility."),
        ("morpho.positions", "Review Morpho V2 shares, underlying backing, withdrawal capacity and indexed earnings."),
        ("morpho.deposit", "Deposit an exact ERC-20 amount into a verified Morpho V2 vault with a share-price bound."),
        ("morpho.withdraw", "Withdraw an exact asset amount from a Morpho V2 vault within a finite share allowance."),
        ("morpho.redeem", "Redeem exact Morpho V2 shares at execution-time value; estimated proceeds have no guaranteed minimum."),
        ("morpho.rewards", "Review Morpho V2 reward rates and wallet-level Merkl pending, claimable and claimed rewards."),
        ("morpho.rewards.claim", "Claim selected Merkl reward tokens to the enrolled wallet, including the full wallet/token aggregate."),
    ].into_iter().map(|(name, description)| {
        let read = is_read(name);
        let mut input_schema = if name == "morpho.rewards" {
            serde_json::to_value(schemars::schema_for!(rewards::RewardsReadInput))
        } else if name == "morpho.rewards.claim" {
            serde_json::to_value(schemars::schema_for!(rewards::RewardClaimInput))
        } else if read {
            serde_json::to_value(schemars::schema_for!(reads::MorphoReadInput))
        } else {
            serde_json::to_value(schemars::schema_for!(vault::VaultWriteInput))
        }.expect("Morpho input schema");
        if let Some(properties) = input_schema["properties"].as_object_mut() {
            properties.insert("chain_id".into(), json!({"type":"integer","enum":[1,8453]}));
            properties.insert("network".into(), json!({"type":"string","enum":["ethereum","evm"]}));
            let decimal = json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":100});
            for field in ["min_shares", "max_shares", "max_fee_eth"] {
                if properties.contains_key(field) { properties.insert(field.into(), decimal.clone()); }
            }
            if properties.contains_key("amount") {
                properties.insert("amount".into(), if name == "morpho.redeem" { json!({"anyOf":[decimal,{"const":"all"}]}) } else {decimal});
            }
            for field in ["vault", "asset", "owner"] {
                if properties.contains_key(field) { properties.insert(field.into(), json!({"type":"string","pattern":"^0x[0-9a-fA-F]{40}$"})); }
            }
            for field in match name {
                "morpho.deposit" => vec!["max_shares", "accept_prevailing_value"],
                "morpho.withdraw" => vec!["min_shares", "accept_prevailing_value"],
                "morpho.redeem" => vec!["min_shares"],
                _ => vec![],
            } { properties.remove(field); }
        }
        let required = input_schema["required"].as_array_mut().expect("required Morpho fields");
        match name {
            "morpho.vault" => required.push(json!("vault")),
            "morpho.deposit" => required.push(json!("min_shares")),
            "morpho.withdraw" => required.push(json!("max_shares")),
            _ => {},
        }
        if name == "morpho.positions" { input_schema["anyOf"] = json!([{"required":["wallet"]},{"required":["owner"]}]); }
        if name == "morpho.rewards" { input_schema["anyOf"] = json!([{"required":["wallet"]},{"required":["vault"]}]); }
        let mut semantics = json!({
            "vault_version":"V2 only; V1 vaults, fee wrappers, force exits and in-kind exits are unsupported.",
            "networks":[1,8453],
            "amounts":"Exact decimal strings and independently verified asset/share decimals.",
            "data_sources":"Morpho API indexed observations, Merkl reward observations, and independently pinned on-chain evidence. Unknown/incomplete observations are not zero.",
            "transaction_builder":"Locally encoded reviewed ABIs; no production MCP dependency, arbitrary calldata or client-supplied proofs.",
            "recipient":"The enrolled wallet owns the shares and receives assets and reward claims.",
            "recovery":"Reconcile the original immutable submission after uncertainty; never replay a write."
        });
        if !read {
            semantics["broker_generated_attempt_input_fields"] = json!(["/request_id"]);
            semantics["plan_allowance_seconds"] = json!(gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
            semantics["plan_permission"] = json!({
                "supported":true,
                "bounds_schema": if name == "morpho.rewards.claim" {
                    serde_json::to_value(schemars::schema_for!(rewards::RewardClaimBounds)).expect("reward bounds")
                } else {
                    serde_json::to_value(schemars::schema_for!(permissions::MorphoBounds)).expect("vault bounds")
                },
                "fixed_fields":"Network, wallet, vault or distributor, asset/share/token identities, recipient and selected operation semantics."
            });
        }
        if name == "morpho.redeem" {
            semantics["payout"] = json!({"basis":"execution-time vault value","guaranteed_minimum":false,"consent":"accept_prevailing_value must be true","all":"Freeze the observed share quantity within a finite max_shares cap; newly arriving shares are not silently added."});
            input_schema["allOf"] = json!([{"properties":{"accept_prevailing_value":{"const":true}},"required":["accept_prevailing_value"]},{"if":{"properties":{"amount":{"const":"all"}},"required":["amount"]},"then":{"required":["max_shares"]}}]);
        }
        if name == "morpho.rewards.claim" {
            semantics["claim_scope"] = json!("Wallet/token cumulative Merkl entitlement; a selected token may include rewards from other protocols or vault versions. The execution-time unclaimed remainder may decrease to zero if another authorized claim settles first; there is no guaranteed minimum. Pending rewards are not claimable. No token approval or operator grant is requested.");
        }
        let example = match name {
            "morpho.vaults" => json!({"network":"ethereum","chain_id":8453,"limit":20}),
            "morpho.vault" => json!({"network":"ethereum","chain_id":8453,"vault":"0x1111111111111111111111111111111111111111"}),
            "morpho.positions" => json!({"network":"ethereum","chain_id":8453,"wallet":"my-wallet"}),
            "morpho.rewards" => json!({"network":"ethereum","chain_id":8453,"wallet":"my-wallet"}),
            "morpho.rewards.claim" => json!({"network":"ethereum","chain_id":8453,"wallet":"my-wallet","tokens":["0x2222222222222222222222222222222222222222"],"request_id":"reward-claim-1","max_fee_eth":"0.0001"}),
            _ => {
                let mut value = json!({"network":"ethereum","chain_id":8453,"wallet":"my-wallet","vault":"0x1111111111111111111111111111111111111111","amount":"1","request_id":"vault-action-1","max_fee_eth":"0.0001"});
                match name {
                    "morpho.deposit" => value["min_shares"] = json!("0.99"),
                    "morpho.withdraw" => value["max_shares"] = json!("1.01"),
                    _ => value["accept_prevailing_value"] = json!(true),
                }
                value
            }
        };
        semantics["examples"] = json!("Illustrative quantities and addresses only; discover exact deployed vault/token identities and fresh limits before use.");
        Definition {
            name:name.into(), description:description.into(),
            kind:if read { OperationKind::Read } else { OperationKind::Write },
            execution_actor:if read { ExecutionActor::AgentReadable } else { ExecutionActor::AgentInvocable },
            input_schema,
            output_schema:if read { json!({"type":"object"}) } else { serde_json::to_value(schemars::schema_for!(PreparedOperation)).expect("prepared schema") },
            error_schema:serde_json::to_value(schemars::schema_for!(GoFinError)).expect("error schema"),
            examples:vec![example],
            coverage:"Evidence is reported separately for synthetic, fork and live verification.".into(),
            completion:"Canonical original transaction receipts, bounded actual costs and protocol-specific token/share or cumulative reward effects establish settlement.".into(),
            semantics,
        }
    }).collect()
}
