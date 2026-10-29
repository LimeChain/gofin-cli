//! Public operation discovery. Native vault assets remain explicit plan inputs.
use crate::protocol::{VaultAction, VaultFamily};
use gofin_domain::{Definition, ExecutionActor, OperationKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const OP_VAULTS: &str = "kamino.vaults";
pub const OP_VAULT: &str = "kamino.vault";
pub const OP_POSITIONS: &str = "kamino.positions";
pub const OP_POSITION: &str = "kamino.position";
pub const OP_QUOTE: &str = "kamino.quote";
pub const OP_DEPOSIT: &str = "kamino.deposit";
pub const OP_WITHDRAW: &str = "kamino.withdraw";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicAmount {
    pub mint: String,
    pub atoms: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicIntent {
    pub wallet: String,
    pub network: String,
    pub cluster: String,
    pub family: VaultFamily,
    pub vault: String,
    pub share_mint: String,
    #[serde(default)]
    pub action: Option<VaultAction>,
    #[serde(default)]
    pub amounts: Vec<PublicAmount>,
    #[serde(default)]
    pub shares_atoms: Option<String>,
    #[serde(default)]
    pub minimum_shares_atoms: Option<String>,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
    #[serde(default)]
    pub stake: bool,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadInput {
    pub wallet: Option<String>,
    pub family: Option<VaultFamily>,
    pub vault: Option<String>,
    pub search: Option<String>,
    pub token_mint: Option<String>,
    pub limit: Option<u16>,
    pub cursor: Option<String>,
}
fn definition(name: &str, description: &str, kind: OperationKind, schema: Value) -> Definition {
    Definition {
        name:name.into(),description:description.into(),execution_actor:if kind==OperationKind::Read {ExecutionActor::AgentReadable}else{ExecutionActor::AgentInvocable},kind,
        input_schema:schema,output_schema:json!({"type":"object"}),error_schema:json!({"type":"object","required":["code","stage","message","next_action"]}),examples:vec![],
        coverage:"Native Kamino lending and liquidity vaults on Solana mainnet-beta, including farm custody. Execution eligibility is reported per vault.".into(),
        completion:"Writes complete only after finalized transaction evidence proves the vault shares, underlying cashflows and costs.".into(),
        semantics:json!({"boundary_parameters":{"type":"object","properties":{"maximum_amounts":{"type":"array","items":{"type":"object","properties":{"mint":{"type":"string"},"atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"}},"required":["mint","atoms"],"additionalProperties":false}},"maximum_shares_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_stake_shares_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"Required when stake=true; explicitly caps shares moved into the associated farm, including any existing shares when a native receipt underperforms its estimate."},"minimum_shares_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$","description":"Minimum native minted share atoms before any farm entitlement rounding; supported only when enforced by the native instruction."},"maximum_network_fee_lamports":{"type":"integer","minimum":1},"maximum_account_funding_lamports":{"type":"integer","minimum":0}},"required":["maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false},"boundary_example":{"maximum_amounts":[{"mint":"<canonical-underlying-mint>","atoms":"1000000"}],"maximum_stake_shares_atoms":"1000000","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":3000000},"pagination":"Each family returns its own cursor; follow a cursor with that explicit family.","families":["lending","liquidity"],"quantity_unit":"exact atomic units","swaps":"Separate operations composed by the agent in a named plan. Native liquidity deposits require both vault assets; withdrawals return native underlying assets.","share_accounting":"Wallet and farm custody are one owned share position. Current underlying composition is separate from share units and historical contribution basis.","withdraw_all":"Resolved once to a concrete owned and withdrawable share quantity in the reviewed binding; never re-evaluated at dispatch.","quote_limits":"Outputs are estimates unless the preview explicitly identifies an enforced on-chain minimum. A share minimum bounds native minted shares before farm entitlement rounding; redeemable whole share atoms can differ, with fractional entitlements retained in protocol evidence.","oversized_transactions":"Fail preparation rather than splitting into unapproved transactions."}),
    }
}
pub fn operation_definitions() -> Vec<Definition> {
    let read = json!({"type":"object","properties":{"wallet":{"type":"string"},"family":{"enum":["lending","liquidity"]},"vault":{"type":"string"},"search":{"type":"string"},"token_mint":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":25},"cursor":{"type":"string"}},"additionalProperties":false});
    let exact = json!({"type":"string","pattern":"^(0|[1-9][0-9]*)$"});
    let mut minimum = exact.clone();
    minimum["description"] = json!(
        "Minimum native minted share atoms before any farm entitlement rounding; supported only when enforced by the native instruction."
    );
    let write = json!({"type":"object","properties":{
        "wallet":{"type":"string"},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"family":{"enum":["lending","liquidity"]},"vault":{"type":"string"},"share_mint":{"type":"string"},"action":{"enum":["deposit","withdraw"]},
        "amounts":{"type":"array","minItems":1,"maxItems":2,"items":{"type":"object","properties":{"mint":{"type":"string"},"atoms":exact},"required":["mint","atoms"],"additionalProperties":false}},
        "shares_atoms":{"type":"string","pattern":"^(all|[1-9][0-9]*)$"},"minimum_shares_atoms":minimum,"stake":{"type":"boolean","default":false},
        "maximum_network_fee_lamports":{"type":"integer","minimum":1},"maximum_account_funding_lamports":{"type":"integer","minimum":0}},
        "required":["wallet","network","cluster","family","vault","share_mint","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false});
    let mut result = Vec::new();
    for (name, desc, required) in [
        (
            OP_VAULTS,
            "Discover Kamino vaults and native deposit assets.",
            vec![],
        ),
        (
            OP_VAULT,
            "Inspect one canonical Kamino vault.",
            vec!["family", "vault"],
        ),
        (
            OP_POSITIONS,
            "Discover all wallet and farm-held Kamino positions.",
            vec!["wallet"],
        ),
        (
            OP_POSITION,
            "Inspect a wallet's position in one vault.",
            vec!["wallet", "family", "vault"],
        ),
    ] {
        let mut schema = read.clone();
        schema["required"] = json!(required);
        result.push(definition(name, desc, OperationKind::Read, schema));
    }
    for (name, desc, kind, action, required) in [
        (
            OP_QUOTE,
            "Preview native vault inputs, outputs, costs and execution restrictions.",
            OperationKind::Read,
            None,
            "action",
        ),
        (
            OP_DEPOSIT,
            "Deposit native underlying assets into a Kamino vault.",
            OperationKind::Write,
            Some("deposit"),
            "amounts",
        ),
        (
            OP_WITHDRAW,
            "Redeem Kamino vault shares into native underlying assets.",
            OperationKind::Write,
            Some("withdraw"),
            "shares_atoms",
        ),
    ] {
        let mut schema = write.clone();
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!(required));
        if let Some(action) = action {
            schema["properties"]["action"] = json!({"const":action});
        }
        result.push(definition(name, desc, kind, schema));
    }
    result
}
