use gofin_domain::{Definition, ExecutionActor, OperationKind};
use serde_json::{Value, json};

pub const OP_POOLS: &str = "meteora.pools";
pub const OP_POOL: &str = "meteora.pool";
pub const OP_POSITIONS: &str = "meteora.positions";
pub const OP_POSITION: &str = "meteora.position";
pub const OP_QUOTE: &str = "meteora.quote";
pub const OP_SWAP: &str = "meteora.swap";
pub const OP_OPEN: &str = "meteora.position.open";
pub const OP_ADD: &str = "meteora.position.add";
pub const OP_REMOVE: &str = "meteora.position.remove";
pub const OP_COLLECT: &str = "meteora.position.collect";
pub const OP_CLOSE: &str = "meteora.position.close";

fn definition(
    name: &str,
    description: &str,
    kind: OperationKind,
    input_schema: Value,
) -> Definition {
    let execution_actor = match &kind {
        OperationKind::Read => ExecutionActor::AgentReadable,
        _ => ExecutionActor::AgentInvocable,
    };
    Definition {
        name: name.into(),
        description: description.into(),
        kind,
        execution_actor,
        input_schema,
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
        examples: vec![],
        coverage: "Existing Meteora DLMM and DAMM v2 pools and wallet positions on Solana mainnet-beta; each write is one atomic transaction subject to Solana message limits.".into(),
        completion: "Read operations return observed protocol state; writes complete only after finalized transaction effects are verified.".into(),
        semantics: json!({
            "network":"solana:mainnet-beta",
            "families":["dlmm","damm_v2"],
            "write_scope":"one_atomic_transaction",
            "position_accounting":"Stable liquidity units are accounting identity; current A/B composition is separate observed backing. Imported contribution history may be unknown. Current composition includes protocol price movement and compounding; fees and rewards become wallet-usable only when a finalized Collect proves the credit.",
            "hook_accounts":"DLMM resolves canonical Token-2022 hook metas, including writable extras, under ordinary debit and signer bounds. Deployed DAMM v2 instructions cannot forward active hook metas and reject those mints before approval.",
            "oversized_graphs":"Candidate graphs that exceed transaction serialization, account, or compute limits fail preparation; the adapter does not split or silently submit extra transactions."
        }),
    }
}

fn read_schema(required: &[&str]) -> Value {
    json!({
        "type":"object",
        "properties":{
            "wallet":{"type":"string"},
            "family":{"enum":["dlmm","damm_v2"]},
            "pool":{"type":"string"},
            "position":{"type":"string"},
            "search":{"type":"string"},
            "limit":{"type":"integer","minimum":1,"maximum":100,"default":25},
            "cursor":{"type":"string"},
            "token_a_mint":{"type":"string"},
            "token_b_mint":{"type":"string"}
        },
        "required":required,
        "additionalProperties":false
    })
}

pub fn operation_definitions() -> Vec<Definition> {
    let liquidity = json!({
        "type":"object",
        "properties":{
            "action":{"enum":["swap","open","add","remove","collect","close"]},
            "wallet":{"type":"string"},
            "network":{"const":"solana"},
            "cluster":{"const":"mainnet-beta"},
            "family":{"enum":["dlmm","damm_v2"]},
            "pool":{"type":"string"},
            "position":{"type":"string"},
            "input_mint":{"type":"string"},
            "output_mint":{"type":"string"},
            "token_a_mint":{"type":"string","minLength":1},
            "token_b_mint":{"type":"string","minLength":1},
            "amount_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},
            "minimum_output_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            "token_a_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            "token_b_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            "minimum_token_a_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            "minimum_token_b_atoms":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            "remove_share_bps":{"type":"integer","minimum":1,"maximum":10000},
            "bin_lower":{"type":"integer"},
            "bin_upper":{"type":"integer"},
            "liquidity_distribution":{"type":"array","minItems":1,"maxItems":70,"items":{"type":"object","properties":{"bin_id":{"type":"integer"},"distribution_x":{"type":"integer","minimum":0,"maximum":10000},"distribution_y":{"type":"integer","minimum":0,"maximum":10000}},"required":["bin_id","distribution_x","distribution_y"],"additionalProperties":false}},
            "maximum_slippage_bps":{"type":"integer","minimum":1,"maximum":10000},
            "maximum_network_fee_lamports":{"type":"integer","minimum":1},
            "maximum_account_funding_lamports":{"type":"integer","minimum":0}
        },
        "required":["wallet","network","cluster","family","pool","maximum_slippage_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],
        "additionalProperties":false
    });
    let mut definitions = vec![
        definition(
            OP_POOLS,
            "List existing Meteora pools.",
            OperationKind::Read,
            read_schema(&[]),
        ),
        definition(
            OP_POOL,
            "Inspect one existing Meteora pool.",
            OperationKind::Read,
            read_schema(&["family", "pool"]),
        ),
        definition(
            OP_POSITIONS,
            "List Meteora positions owned by a wallet.",
            OperationKind::Read,
            read_schema(&["wallet", "family"]),
        ),
        definition(
            OP_POSITION,
            "Inspect one Meteora position.",
            OperationKind::Read,
            read_schema(&["family", "position"]),
        ),
        definition(
            OP_QUOTE,
            "Preview an exact Meteora swap or position action.",
            OperationKind::Read,
            {
                let mut schema = liquidity.clone();
                schema["required"] = json!([
                    "action",
                    "wallet",
                    "network",
                    "cluster",
                    "family",
                    "pool",
                    "maximum_slippage_bps",
                    "maximum_network_fee_lamports",
                    "maximum_account_funding_lamports"
                ]);
                schema["allOf"] = json!([
                    {"if":{"properties":{"action":{"const":"swap"}}},"then":{"required":["input_mint","output_mint","amount_atoms"]}},
                    {"if":{"properties":{"action":{"enum":["open","add","remove","collect","close"]}}},"then":{"required":["token_a_mint","token_b_mint"]}}
                ]);
                schema
            },
        ),
        definition(
            OP_SWAP,
            "Swap an exact input amount through an existing Meteora pool. Use the minimum_output_atoms recommended by meteora.quote for the same pool, input and slippage.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "swap",
                &[
                    "input_mint",
                    "output_mint",
                    "amount_atoms",
                    "minimum_output_atoms",
                ],
            ),
        ),
        definition(
            OP_OPEN,
            "Open a Meteora liquidity position in an existing pool.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "open",
                &[
                    "token_a_mint",
                    "token_b_mint",
                    "token_a_atoms",
                    "token_b_atoms",
                ],
            ),
        ),
        definition(
            OP_ADD,
            "Add exact token amounts to a Meteora liquidity position.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "add",
                &[
                    "position",
                    "token_a_mint",
                    "token_b_mint",
                    "token_a_atoms",
                    "token_b_atoms",
                ],
            ),
        ),
        definition(
            OP_REMOVE,
            "Remove a bounded share of Meteora liquidity. Set each token minimum from the corresponding meteora.quote output and approved slippage.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "remove",
                &[
                    "position",
                    "token_a_mint",
                    "token_b_mint",
                    "remove_share_bps",
                    "minimum_token_a_atoms",
                    "minimum_token_b_atoms",
                ],
            ),
        ),
        definition(
            OP_COLLECT,
            "Collect accrued Meteora fees and rewards.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "collect",
                &["position", "token_a_mint", "token_b_mint"],
            ),
        ),
        definition(
            OP_CLOSE,
            "Close a Meteora position and return its remaining assets and rent. The zero-output example applies only to a position whose quote confirms both remaining token amounts are zero.",
            OperationKind::Write,
            action_schema(
                &liquidity,
                "close",
                &[
                    "position",
                    "token_a_mint",
                    "token_b_mint",
                    "minimum_token_a_atoms",
                    "minimum_token_b_atoms",
                ],
            ),
        ),
    ];
    for definition in &mut definitions {
        if definition.kind == OperationKind::Write {
            definition.semantics["write_scope"] = json!("one_atomic_transaction");
            if definition.name == OP_REMOVE {
                definition.semantics["minimum_output_source"] = json!({
                    "token_a":"meteora.quote.recommended_floors.minimum_token_a_atoms",
                    "token_b":"meteora.quote.recommended_floors.minimum_token_b_atoms"
                });
            }
            let supported = definition.name != OP_OPEN;
            definition.semantics["plan_permission"] = json!({
                "requires_adapter_scope": true,
                "supported": supported,
                "scope": if supported {
                    "fixed_wallet_pool_assets_and_position_with_amount_caps"
                } else {
                    "unsupported_generated_position_identity"
                },
                "notes": if supported {
                    "Meteora plan steps pin wallet, family, pool, token mints, and existing position; execution revalidates current balances and finalized effects."
                } else {
                    "Open requires a broker-generated position identity that is not yet available at plan approval."
                }
            });
        }
        definition.output_schema = match definition.name.as_str() {
            OP_POOLS => {
                json!({"type":"object","required":["pools","complete","next_cursor","observed_at"],"properties":{"pools":{"type":"array"},"complete":{"type":"boolean"},"next_cursor":{"type":["string","null"]},"observed_at":{"type":"string"}}})
            }
            OP_POOL => {
                json!({"type":"object","required":["pool","observed_at"],"properties":{"pool":{"type":"object"},"observed_at":{"type":"string"}}})
            }
            OP_POSITIONS => {
                json!({"type":"object","required":["positions","complete","coverage","observed_at"],"properties":{"positions":{"type":"array"},"complete":{"type":"boolean"},"coverage":{"type":"string"},"observed_at":{"type":"string"}}})
            }
            OP_POSITION => {
                json!({"type":"object","required":["position","observed_at"],"properties":{"position":{"type":"object"},"observed_at":{"type":"string"}}})
            }
            OP_QUOTE => {
                json!({"type":"object","required":["action","quote","output_mint","output_mints","recommended_floors","transaction_requirements","observed_at"],"properties":{"action":{"enum":["swap","open","add","remove","collect","close"]},"quote":{"type":"object"},"output_mint":{"type":["string","null"]},"output_mints":{"type":"array"},"recommended_floors":{"type":"object"},"transaction_requirements":{"type":"object","required":["required_accounts","expected_network_fee_lamports","maximum_network_fee_lamports","account_funding_lamports","retained_rent_lamports","refundable_rent_lamports","temporary_wsol_rent_lamports","temporary_wsol_principal_lamports","required_starting_sol_lamports","available_sol_lamports","remaining_sol_after_requirements_lamports","rent_estimate_complete","funding_within_approved_cap"],"properties":{"required_accounts":{"type":"array"},"rent_accounts":{"type":"array"},"unknown_rent_accounts":{"type":"array"}}},"observed_at":{"type":"string"}}})
            }
            _ => {
                json!({"type":"object","required":["operation_id","status"],"properties":{"operation_id":{"type":"string"},"status":{"type":"string"}}})
            }
        };
        definition.examples = match definition.name.as_str() {
            OP_POOLS => vec![
                json!({"family":"dlmm","limit":20,"token_a_mint":"So11111111111111111111111111111111111111112"}),
            ],
            OP_POOL => vec![json!({"family":"damm_v2","pool":"<pool-address>"})],
            OP_POSITIONS => vec![json!({"wallet":"main","family":"dlmm"})],
            OP_POSITION => vec![json!({"family":"dlmm","position":"<position-address>"})],
            OP_QUOTE => vec![
                json!({"action":"swap","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"<pool-address>","input_mint":"So11111111111111111111111111111111111111112","output_mint":"<output-mint>","amount_atoms":"1000000","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
                json!({"action":"remove","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"<pool-address>","position":"<position-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","remove_share_bps":5000,"maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            OP_SWAP => vec![
                json!({"action":"swap","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"<pool-address>","input_mint":"So11111111111111111111111111111111111111112","output_mint":"<output-mint>","amount_atoms":"1000000","minimum_output_atoms":"995000","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            OP_OPEN => vec![
                json!({"action":"open","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"<pool-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","token_a_atoms":"1000000","token_b_atoms":"1000000","bin_lower":-10,"bin_upper":10,"liquidity_distribution":[{"bin_id":-10,"distribution_x":10000,"distribution_y":0},{"bin_id":10,"distribution_x":0,"distribution_y":10000}],"maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":10000000}),
            ],
            OP_ADD => vec![
                json!({"action":"add","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"damm_v2","pool":"<pool-address>","position":"<position-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","token_a_atoms":"1000000","token_b_atoms":"1000000","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            OP_REMOVE => vec![
                json!({"action":"remove","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"<pool-address>","position":"<position-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","remove_share_bps":1000,"minimum_token_a_atoms":"0","minimum_token_b_atoms":"0","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            OP_COLLECT => vec![
                json!({"action":"collect","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"damm_v2","pool":"<pool-address>","position":"<position-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            OP_CLOSE => vec![
                json!({"action":"close","wallet":"main","network":"solana","cluster":"mainnet-beta","family":"damm_v2","pool":"<pool-address>","position":"<position-address>","token_a_mint":"<pool-token-a-mint>","token_b_mint":"<pool-token-b-mint>","minimum_token_a_atoms":"0","minimum_token_b_atoms":"0","maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),
            ],
            _ => vec![],
        };
    }
    definitions
}

fn action_schema(schema: &Value, action: &str, extra_required: &[&str]) -> Value {
    let mut schema = schema.clone();
    schema["properties"]["action"] = json!({"const":action});
    let mut required = vec![
        "action",
        "wallet",
        "network",
        "cluster",
        "family",
        "pool",
        "maximum_slippage_bps",
        "maximum_network_fee_lamports",
        "maximum_account_funding_lamports",
    ];
    required.extend_from_slice(extra_required);
    schema["required"] = json!(required);
    schema
}
