//! Collateral-only Perps operations.
//!
//! These are distinct from a position-size increase/decrease: the derivative
//! notional remains unchanged while only the locked collateral moves.

use super::*;

pub(super) fn operation_definitions() -> Vec<Definition> {
    let common_output = json!({
        "type":"object",
        "properties":{
            "state":{"enum":["request_submitted","keeper_pending","settled"]},
            "request":{"type":"string"},
            "position":{"type":"string"},
            "collateral_delta_usd_atoms":{"type":"string"},
            "collateral_asset":{"type":"string"},
            "signature":{"type":"string"},
            "settlement":{"type":"object"}
        },
        "required":["state","request","position","collateral_delta_usd_atoms","collateral_asset"],
        "additionalProperties":false
    });
    vec![
        Definition {
            name: OP_COLLATERAL_ADD.into(),
            description: "Add wallet-funded collateral to an existing Jupiter Perps position without increasing its notional.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"position":{"type":"string","minLength":32,"maxLength":44},"market":{"enum":["SOL","ETH","BTC"]},"side":{"enum":["long","short"]},"funding_token":{"enum":["SOL","ETH","BTC","USDC"]},"collateral_amount_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_protocol_fee_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":1,"maximum":5000},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$"}},"required":["network","cluster","wallet","position","market","side","funding_token","collateral_amount_atoms","maximum_protocol_fee_atoms","maximum_protocol_fee_usd_atoms","maximum_slippage_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false}),
            output_schema: common_output.clone(),
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","position":"11111111111111111111111111111111","market":"SOL","side":"long","funding_token":"USDC","collateral_amount_atoms":"1000000","maximum_protocol_fee_atoms":"10000","maximum_protocol_fee_usd_atoms":"10000","maximum_slippage_bps":100,"maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})],
            coverage: "Provider quote and pinned instant increase request are bound to the selected existing position; notional must remain unchanged while finalized collateral and fee effects settle.".into(),
            completion: "A submitted collateral request remains pending until its exact keeper event and finalized Position anchor prove the collateral-only change.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"createIncreasePositionMarketRequest","sizeUsdDelta":"0","position_effect":"collateral_only"}),
        },
        Definition {
            name: OP_COLLATERAL_REMOVE.into(),
            description: "Withdraw collateral from an existing Jupiter Perps position without decreasing its notional.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"position":{"type":"string","minLength":32,"maxLength":44},"receive_token":{"enum":["SOL","ETH","BTC","USDC"]},"collateral_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"minimum_receive_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":1,"maximum":5000},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$"}},"required":["network","cluster","wallet","position","receive_token","collateral_usd_atoms","minimum_receive_atoms","maximum_protocol_fee_usd_atoms","maximum_slippage_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false}),
            output_schema: common_output,
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","position":"11111111111111111111111111111111","receive_token":"USDC","collateral_usd_atoms":"1000000","minimum_receive_atoms":"900000","maximum_protocol_fee_usd_atoms":"10000","maximum_slippage_bps":100,"maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})],
            coverage: "Provider quote and pinned instant decrease request are bound to the selected existing position; sizeUsdDelta must remain zero while finalized collateral payout and fees settle.".into(),
            completion: "A submitted collateral withdrawal remains pending until its exact keeper event, token deltas and finalized Position anchor prove the collateral-only change.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"createDecreasePositionRequest2","sizeUsdDelta":"0","position_effect":"collateral_only"}),
        },
    ]
}
