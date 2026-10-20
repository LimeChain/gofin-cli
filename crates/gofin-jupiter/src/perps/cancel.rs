//! Exact parent-bound cancellation for a Jupiter Perps limit request.
//!
//! The cancellation transaction proves that the named request was closed.
//! It never books the request's collateral or account rent: those cumulative
//! refunds and any racing keeper fill belong to the original create record.

use super::*;
use crate::adapter;

pub(super) const EXECUTE_ACTION: &str = "cancel-limit-order";
const CLOSE_REQUEST: &str = "closePositionRequest3";

pub(super) fn operation_definition() -> Definition {
    Definition {
        name: OP_LIMIT_CANCEL.into(),
        description: "Cancel one open Jupiter Perps limit request by binding the exact unresolved create operation and submission.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: json!({
            "type":"object",
            "properties":{
                "network":{"const":"solana"},
                "cluster":{"const":CLUSTER},
                "wallet":{"type":"string","minLength":1},
                "request":{"anyOf":[{"type":"string","minLength":32,"maxLength":44},{"type":"null"}],"description":"Exact request PDA; null only for a named-plan recovery placeholder."},
                "original_operation_id":{"anyOf":[{"type":"string","minLength":1},{"type":"null"}],"description":"Exact pending jupiter.perps.limit.create operation ID from GoFin status; null only for a named-plan recovery placeholder."},
                "original_submission":{"anyOf":[{"type":"object","properties":{"kind":{"type":"string"},"reference":{"type":"string"},"data":{"type":"object"}},"required":["kind","reference","data"],"additionalProperties":true},{"type":"null"}],"description":"Optional exact submission echo. GoFin normally loads the immutable submission from the local operation journal; named-plan recovery fills this field itself."},
                "maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"}
            },
            "required":["network","cluster","wallet","request","original_operation_id","maximum_network_fee_lamports"],
            "additionalProperties":false
        }),
        output_schema: json!({
            "type":"object",
            "properties":{
                "state":{"enum":["cancel_submitted","cancelled"]},
                "request":{"type":"string"},
                "original_operation_id":{"type":"string"},
                "signature":{"type":"string"},
                "slot":{"type":"integer"},
                "proof_digest":{"type":"string"},
                "accounting_owner":{"const":"original_create_operation"}
            },
            "required":["state","request","original_operation_id"],
            "additionalProperties":false
        }),
        error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
        examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","request":"11111111111111111111111111111111","original_operation_id":"operation-from-gofin-status","maximum_network_fee_lamports":"10000"})],
        coverage: "Cancellation is bound to the original durable limit-create submission and verified against the pinned closePositionRequest3 account graph, request state, event, token movements and finalized rent refund.".into(),
        completion: "Cancellation completion proves only that the request was closed; the original create record remains responsible for keeper fills, escrow refund and refundable account funding.".into(),
        semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":CLOSE_REQUEST,"parent_operation":OP_LIMIT_CREATE,"parent_binding":"authoritative durable create submission; caller-supplied binding is ignored","order_lifecycle":"original_create_record_owns_fills_and_refunds","uncertain_submission":"reconcile_original_cancel_identity_never_retry"}),
    }
}

pub(super) async fn permission_scope(
    adapter: &PerpsAdapter,
    input: &Value,
    bounds: &Value,
) -> Result<PlanPermission> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let recovery_step = bounds
        .get("recovery_parent_step_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let supplied_parent = input
        .get("original_operation_id")
        .is_some_and(|value| !value.is_null())
        || input
            .get("original_submission")
            .is_some_and(|value| !value.is_null());
    let supplied_request = input
        .get("request")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let (parent_id, parent_submission, request, recovery_parent) = if let Some(step_id) =
        recovery_step
    {
        if supplied_parent || supplied_request.is_some() {
            return Err(GoFinError::validation(
                "For a named-plan limit cancellation, omit the original create ID, submission and request; GoFin binds them from the create step.",
            ));
        }
        (
            "__gofin_plan_recovery_parent__".to_owned(),
            Submission {
                kind: "plan_recovery_parent".into(),
                reference: step_id.to_owned(),
                data: json!({"recovery_parent":true}),
            },
            "__gofin_plan_recovery_request__".to_owned(),
            Some(gofin_domain::financial_plans::FinancialPlanRecoveryParent {
                step_id: step_id.to_owned(),
                operation_id_pointer: "/original_operation_id".into(),
                submission_pointer: "/original_submission".into(),
                parent_binding_fields: vec![
                    gofin_domain::financial_plans::FinancialPlanRecoveryBinding {
                        parent_binding_pointer: "/request".into(),
                        child_input_pointer: "/request".into(),
                    },
                ],
            }),
        )
    } else {
        let request = supplied_request
            .ok_or_else(|| GoFinError::validation("Select the exact open Perps limit request."))?;
        gofin_solana::parse_address(request)?;
        let parent_id = required_text(input, "original_operation_id")?;
        let (parent, submission) =
            adapter.limit_create_parent(parent_id, wallet.address(), request)?;
        if let Some(echo) = input
            .get("original_submission")
            .filter(|value| !value.is_null())
        {
            let echo: Submission = serde_json::from_value(echo.clone()).map_err(|_| {
                GoFinError::validation("The original submission echo is malformed.")
            })?;
            if echo != submission {
                return Err(unreviewed_detail(
                    "the submission echo differs from the durable create record",
                ));
            }
        }
        if parent.operation != OP_LIMIT_CREATE {
            return Err(unreviewed_detail(
                "the recovery parent is not a Jupiter limit create",
            ));
        }
        (parent_id.to_owned(), submission, request.to_owned(), None)
    };
    let max_fee = bounds
        .get("maximum_network_fee_lamports")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| GoFinError::validation("Set a positive limit-cancel network-fee cap."))?;
    let resource = resource_for_wallet(wallet.address());
    let native = asset("native", 9);
    Ok(PlanPermission {
        spec: BoundarySpec {
            fixed: json!({
                "operation":OP_LIMIT_CANCEL,"network":NETWORK,"cluster":CLUSTER,
                "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":input["wallet"],
                "wallet_address":wallet.address(),"request":request,
                "parent_operation_id":parent_id,"parent_submission":parent_submission,
                "recovery_parent":recovery_step.is_some()
            }),
            parameters: json!({"maximum_network_fee_lamports":max_fee.to_string()}),
            maximum_debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset: native.clone(),
                    atoms: gofin_domain::Quantity::from_u64(max_fee),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            summary: format!(
                "Authorize cancellation of this exact Jupiter Perps limit request, with network fees up to {max_fee} lamports."
            ),
        },
        recovery_parent,
        possible_credits: vec![],
        fee_assets: vec![native],
        deferred_receipt: None,
        authorization_seconds: Some(600),
        cumulative_scope: None,
    })
}

pub(super) fn validate_permission_candidate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let fixed = &spec.fixed;
    let binding = &prepared.binding;
    let recovery_mode = fixed.get("recovery_parent").and_then(Value::as_bool) == Some(true);
    for (key, actual) in [
        ("operation", json!(prepared.operation)),
        ("network", json!(NETWORK)),
        ("cluster", json!(CLUSTER)),
        ("program", json!(crate::jlp::PERPS_PROGRAM_ID)),
        (
            "wallet_name",
            prepared.input.get("wallet").cloned().unwrap_or(Value::Null),
        ),
        (
            "wallet_address",
            binding
                .get("wallet_address")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    ] {
        if fixed.get(key) != Some(&actual) {
            return Err(GoFinError::validation(
                "Perps limit cancellation differs from the authorized operation or wallet.",
            ));
        }
    }
    if !gofin_domain::product_names::recovery_placeholder(
        fixed.get("request").and_then(Value::as_str),
        "__gofin_plan_recovery_request__",
    ) && fixed.get("request") != binding.get("request")
    {
        return Err(GoFinError::validation(
            "Perps limit cancellation request differs from the authorized request.",
        ));
    }
    if !recovery_mode
        && (fixed.get("parent_operation_id") != binding.get("parent_operation_id")
            || fixed.get("parent_submission") != binding.get("parent_submission"))
    {
        return Err(GoFinError::validation(
            "Perps limit cancellation is bound to a different create submission.",
        ));
    }
    let maximum_fee = spec.parameters["maximum_network_fee_lamports"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    if parse_binding_u64(binding, "maximum_network_fee_lamports")? > maximum_fee {
        return Err(GoFinError::validation(
            "Perps limit cancellation exceeds its approved network-fee cap.",
        ));
    }
    Ok(())
}

pub(super) async fn prepare(adapter: &PerpsAdapter, input: &Value) -> Result<PreparedOperation> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let request = required_text(input, "request")?.to_owned();
    gofin_solana::parse_address(&request)?;
    let parent_operation_id = required_text(input, "original_operation_id")?.to_owned();
    if parent_operation_id.trim().is_empty() {
        return Err(GoFinError::validation(
            "Perps cancellation requires the exact original create operation identity.",
        ));
    }
    let parent_record = adapter
        .parent_operation(&parent_operation_id)?
        .ok_or_else(|| {
            unreviewed_detail("the original create operation is not in the local execution journal")
        })?;
    let parent_submission = parent_record
        .submission
        .as_ref()
        .ok_or_else(|| unreviewed_detail("the original create has no durable submission"))?;
    if let Some(supplied) = input
        .get("original_submission")
        .filter(|value| !value.is_null())
    {
        let supplied: Submission = serde_json::from_value(supplied.clone()).map_err(|_| {
            GoFinError::validation("The optional original Perps submission echo is malformed.")
        })?;
        if &supplied != parent_submission {
            return Err(unreviewed_detail(
                "the optional submission echo differs from the original durable submission",
            ));
        }
    }
    let parent_submission = parent_submission.clone();
    if parent_record.operation != OP_LIMIT_CREATE
        || parent_record.status.terminal()
        || !matches!(
            &parent_record.status,
            Status::Submitted | Status::Pending | Status::Unknown
        )
        || parent_record.submission.as_ref() != Some(&parent_submission)
    {
        return Err(unreviewed_detail(
            "the original create operation is terminal, unresolved identity differs, or submission is not authoritative",
        ));
    }
    let parent_binding = parent_record
        .preview
        .as_ref()
        .map(|preview| &preview.binding)
        .ok_or_else(|| unreviewed_detail("the original create binding is unavailable"))?;
    if parent_binding.get("operation").and_then(Value::as_str) != Some(OP_LIMIT_CREATE)
        || parent_binding.get("request").and_then(Value::as_str) != Some(request.as_str())
        || parent_binding.get("wallet_address").and_then(Value::as_str) != Some(wallet.address())
    {
        return Err(unreviewed_detail(
            "the saved create binding does not match this request and wallet",
        ));
    }
    let maximum_network_fee_lamports = required_u64(input, "maximum_network_fee_lamports")?;
    if maximum_network_fee_lamports == 0 {
        return Err(GoFinError::validation(
            "A positive network-fee limit is required for a Perps request cancellation.",
        ));
    }

    let existing = adapter
        .position_request_by_address(&wallet, &request)
        .await?;
    if existing.request_type != 1 || existing.executed {
        return Err(GoFinError::new(
            "JUPITER_PERPS_LIMIT_NOT_OPEN",
            "prepare",
            "The exact Perps request is no longer an open, unexecuted limit request.",
            "Reconcile the original request and any keeper fill before preparing cancellation.",
        ));
    }
    if parent_binding.get("operation").and_then(Value::as_str) != Some(OP_LIMIT_CREATE)
        || parent_binding.get("request").and_then(Value::as_str) != Some(request.as_str())
        || parent_binding.get("wallet_address").and_then(Value::as_str) != Some(wallet.address())
        || parent_binding.get("position").and_then(Value::as_str)
            != Some(existing.position.as_str())
        || parent_binding.get("funding_mint").and_then(Value::as_str)
            != Some(existing.mint.as_str())
        || parent_binding.get("counter").and_then(Value::as_str)
            != Some(existing.counter.to_string().as_str())
    {
        return Err(unreviewed_detail(
            "parent create binding does not match the exact open request",
        ));
    }

    let response = adapter
        .api
        .request(
            JupiterHost::Perps,
            Method::DELETE,
            "/orders/limit",
            Some(json!({"positionRequestPubkey":request})),
            RateBucket::Main,
            RequestPriority::Financial,
            DEADLINE,
        )
        .await?;
    if response
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .is_some_and(|returned| returned != request)
    {
        return Err(unreviewed_detail("cancel response request identity"));
    }
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(bad_response)?;
    let metadata = response
        .get("txMetadata")
        .filter(|value| value.is_object())
        .ok_or_else(bad_response)?;
    let last_valid_block_height = parse_u64_text(metadata, "lastValidBlockHeight")?;
    let missing = gofin_solana::transaction::SolanaRuntime::missing_signers(encoded)?;
    // The wallet's own slot is necessarily empty in a provider transaction;
    // only foreign required signers belong in the runtime's cosigner allowlist.
    let missing_cosigners = external_cancel_cosigners(wallet.address(), &missing)?;
    let approved_external_cosigners =
        limit_create::foreign_required_signers(encoded, wallet.address())?;
    let allowed = missing_cosigners.clone();
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            last_valid_block_height,
            maximum_network_fee_lamports,
            None,
            &allowed,
        )
        .await?;
    let token_program = adapter::token_program_for(&wallet.runtime(), &existing.mint).await?;
    let mint_decimals = adapter::token_decimals(&wallet.runtime(), &existing.mint).await?;
    let instruction = inspect_cancel_instruction(
        &wallet,
        &frozen,
        &existing,
        &missing_cosigners,
        &token_program,
    )?;
    let auxiliary = adapter::validate_auxiliary_instructions_with_primary_program(
        &wallet,
        &frozen,
        std::slice::from_ref(&existing.mint),
        None,
        0,
        Some(crate::jlp::PERPS_PROGRAM_ID),
    )
    .await?;
    if auxiliary.account_funding_lamports != 0
        || auxiliary.wrapped_native_lamports != 0
        || !auxiliary.closed_wrapped_native_accounts.is_empty()
        || !auxiliary.funding_accounts.is_empty()
    {
        return Err(unreviewed_detail(
            "cancel transaction requires unapproved account funding or native wrapping",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;

    let resource = resource_for_wallet(wallet.address());
    let fee_bound = CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: asset("native", 9),
            atoms: gofin_domain::Quantity::from_u64(maximum_network_fee_lamports),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let binding = json!({
        "operation":OP_LIMIT_CANCEL,
        "wallet_address":wallet.address(),
        "request":request,
        "position":existing.position,
        "market_mint":parent_binding.get("market_mint"),
        "request_mint":existing.mint,
        "request_token_account":instruction.request_token_account,
        "owner_token_account":instruction.owner_token_account,
        "token_program":token_program,
        "mint_decimals":mint_decimals,
        "close_instruction_data":instruction.data_base64,
        "close_instruction_accounts":instruction.accounts,
        "request_change":existing.request_change,
        "request_type":existing.request_type,
        "side":existing.side,
        "amount_atoms":existing.collateral_delta.to_string(),
        "counter":existing.counter.to_string(),
        "parent_operation_id":parent_operation_id,
        "parent_submission":parent_submission,
        "original_operation_id":parent_operation_id,
        "original_submission":parent_submission,
        "parent_message_sha256":parent_binding.get("message_sha256"),
        "request_rent_lamports":parent_binding.get("request_rent_lamports"),
        "transaction_base64":encoded,
        "message_sha256":frozen.message_sha256(),
        "last_valid_block_height":last_valid_block_height,
        "fee_payer":frozen.fee_payer(),
        "maximum_network_fee_lamports":maximum_network_fee_lamports.to_string(),
        "keeper_signer":instruction.keeper_signer,
        "approved_external_cosigners":approved_external_cosigners,
    });
    let (valuation, _) = adapter::valuation(
        adapter.prices.clone(),
        asset("native", 9),
        0,
        asset("native", 9),
        0,
        maximum_network_fee_lamports,
        wallet.address(),
    )
    .await?;
    Ok(PreparedOperation {
        operation: OP_LIMIT_CANCEL.into(),
        input: input.clone(),
        binding,
        summary: format!(
            "Cancel the open Jupiter Perps limit request {request}. The original request record remains responsible for any keeper fill, returned collateral and rent.",
        ),
        checks: vec![
            check("Exact original request and create submission binding"),
            check("Pinned closePositionRequest3 account graph and signer roles"),
            check("Network fee cap and transaction simulation"),
        ],
        warnings: vec![
            "Cancellation may race a keeper fill. GoFin reconciles both transactions against the same request; it never retries an uncertain cancellation.".into(),
        ],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
        valuation: Some(valuation),
        intent: FinancialIntent {
            debits: vec![],
            maximum_costs: vec![fee_bound],
            expected: vec![EffectConstraint::SubmissionCancellation {
                operation_id: parent_operation_id,
                submission: parent_submission,
            }],
            // Cancellation shares only the exact wallet/request/position lease
            // held by the authoritative create parent. The position comes from
            // the verified on-chain request above, after it was matched to the
            // immutable parent binding.
            locks: vec![
                resource,
                ResourceId(format!("solana:mainnet-beta:jupiter-perps-request:{request}")),
                super::position_lock_resource(&existing.position),
            ],
        },
        action_deadline: None,
            operation_version: 1,
    })
}

pub(super) fn validate_parent_aliases(binding: &Value) -> Result<()> {
    let operation_id = binding.get("original_operation_id");
    let submission = binding.get("original_submission");
    if operation_id.is_none() && submission.is_none() {
        // Read older durable records through the authoritative parent fields.
        return Ok(());
    }
    let alias_submission: Submission =
        serde_json::from_value(submission.cloned().ok_or_else(bad_response)?)
            .map_err(|_| bad_response())?;
    if operation_id.and_then(Value::as_str)
        != binding.get("parent_operation_id").and_then(Value::as_str)
        || binding.get("parent_submission")
            != Some(&serde_json::to_value(alias_submission).map_err(|_| bad_response())?)
    {
        return Err(unreviewed_detail(
            "the limit-cancel recovery aliases differ from the authoritative parent binding",
        ));
    }
    Ok(())
}

pub(super) struct CancelInstruction {
    pub(super) owner_token_account: String,
    pub(super) request_token_account: String,
    pub(super) keeper_signer: Option<String>,
    pub(super) data_base64: String,
    pub(super) accounts: Vec<Value>,
}

pub(super) fn inspect_cancel_instruction(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    request: &VerifiedRequest,
    missing: &[String],
    token_program: &str,
) -> Result<CancelInstruction> {
    let instructions = frozen.instructions();
    let primary = instructions
        .iter()
        .filter(|instruction| instruction.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if primary.len() != 1 {
        return Err(unreviewed_detail(
            "expected exactly one closePositionRequest3 instruction",
        ));
    }
    let instruction = primary[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| bad_response())?;
    if data != instruction_discriminator(CLOSE_REQUEST) || instruction.accounts.len() != 14 {
        return Err(unreviewed_detail(
            "closePositionRequest3 discriminator or layout",
        ));
    }
    let accounts = &instruction.accounts;
    let program = crate::jlp::PERPS_PROGRAM_ID;
    let keeper_is_optional = accounts[0].address == program;
    let keeper_signer = (!keeper_is_optional).then(|| accounts[0].address.clone());
    if (keeper_is_optional && (accounts[0].signer || accounts[0].writable))
        || (!keeper_is_optional && (!accounts[0].signer || !accounts[0].writable))
    {
        return Err(unreviewed_detail("optional keeper signer role"));
    }
    let owner_token_account =
        gofin_solana::associated_token_address(wallet.address(), &request.mint, token_program)?;
    let request_token_account =
        gofin_solana::associated_token_address(&request.address, &request.mint, token_program)?;
    let expected = [
        None,
        Some(wallet.address().to_owned()),
        Some(owner_token_account.clone()),
        Some(request.pool.clone()),
        Some(request.address.clone()),
        Some(request_token_account.clone()),
        Some(request.position.clone()),
        Some(request.custody.clone()),
        Some(request.mint.clone()),
        Some(token_program.to_owned()),
        Some("11111111111111111111111111111111".into()),
        Some("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".into()),
        Some(pda(&[b"__event_authority"])?),
        Some(program.into()),
    ];
    for (index, (account, address)) in accounts.iter().zip(expected).enumerate() {
        if address
            .as_deref()
            .is_some_and(|address| account.address != address)
        {
            return Err(unreviewed_detail("closePositionRequest3 account identity"));
        }
        // Compiled Solana account privileges are the union of every role in
        // the message. The owner slot is nonsigning in this IDL, but becomes a
        // signer when that wallet also occupies the fee-payer or reviewed
        // keeper signer role.
        let should_sign = cancel_account_should_sign(
            index,
            keeper_is_optional,
            &account.address,
            frozen.fee_payer(),
            keeper_signer.as_deref(),
        );
        let should_write = cancel_account_should_write(index, keeper_is_optional);
        if account.signer != should_sign || account.writable != should_write {
            return Err(unreviewed_detail("closePositionRequest3 account privilege"));
        }
    }
    if missing.len() > 1
        || missing
            .iter()
            .any(|signer| Some(signer.as_str()) != keeper_signer.as_deref())
        || frozen.required_signers().iter().any(|signer| {
            signer != wallet.address() && Some(signer.as_str()) != keeper_signer.as_deref()
        })
    {
        return Err(unreviewed_detail(
            "unexpected cancellation transaction signer",
        ));
    }
    Ok(CancelInstruction {
        owner_token_account,
        request_token_account,
        keeper_signer,
        data_base64: instruction.data_base64.clone(),
        accounts: instruction
            .accounts
            .iter()
            .map(|account| {
                json!({
                    "address":account.address,
                    "signer":account.signer,
                    "writable":account.writable
                })
            })
            .collect(),
    })
}

fn cancel_account_should_sign(
    index: usize,
    keeper_is_optional: bool,
    address: &str,
    fee_payer: &str,
    keeper_signer: Option<&str>,
) -> bool {
    (index == 0 && !keeper_is_optional)
        || (index == 1 && (address == fee_payer || keeper_signer == Some(address)))
}

fn cancel_account_should_write(index: usize, keeper_is_optional: bool) -> bool {
    (index == 0 && !keeper_is_optional) || matches!(index, 1 | 2 | 3 | 4 | 5 | 7)
}

pub(super) async fn revalidate(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
    if frozen.message_sha256()
        != prepared.binding["message_sha256"]
            .as_str()
            .ok_or_else(bad_response)?
    {
        return Err(unreviewed());
    }
    let request = prepared.binding["request"]
        .as_str()
        .ok_or_else(bad_response)?;
    let existing = adapter
        .position_request_by_address(&wallet, request)
        .await?;
    let parent_operation_id = prepared.binding["parent_operation_id"]
        .as_str()
        .ok_or_else(bad_response)?;
    validate_parent_aliases(&prepared.binding)?;
    let parent = adapter
        .parent_operation(parent_operation_id)?
        .ok_or_else(|| {
            unreviewed_detail("original create operation disappeared from the local journal")
        })?;
    let expected_parent_submission: Submission =
        serde_json::from_value(prepared.binding["parent_submission"].clone())
            .map_err(|_| bad_response())?;
    let parent_binding = parent
        .preview
        .as_ref()
        .map(|preview| &preview.binding)
        .ok_or_else(bad_response)?;
    if parent.operation != OP_LIMIT_CREATE
        || parent.status.terminal()
        || !matches!(
            &parent.status,
            Status::Submitted | Status::Pending | Status::Unknown
        )
        || parent.submission.as_ref() != Some(&expected_parent_submission)
        || parent_binding["request"] != prepared.binding["request"]
        || parent_binding["wallet_address"] != prepared.binding["wallet_address"]
        || parent_binding["message_sha256"] != prepared.binding["parent_message_sha256"]
    {
        return Err(unreviewed_detail(
            "the exact create attempt is no longer the active unresolved parent",
        ));
    }
    if existing.executed
        || existing.request_type != 1
        || existing.position
            != prepared.binding["position"]
                .as_str()
                .ok_or_else(bad_response)?
        || existing.counter.to_string()
            != prepared.binding["counter"]
                .as_str()
                .ok_or_else(bad_response)?
    {
        return Err(GoFinError::new(
            "JUPITER_PERPS_LIMIT_NOT_OPEN",
            "revalidation",
            "The exact Perps request changed or is no longer open.",
            "Reconcile the original create and cancellation records; prepare a new action only after final state is known.",
        ));
    }
    let missing = gofin_solana::transaction::SolanaRuntime::missing_signers(
        prepared.binding["transaction_base64"]
            .as_str()
            .ok_or_else(bad_response)?,
    )?;
    let missing_cosigners = external_cancel_cosigners(wallet.address(), &missing)?;
    let token_program = adapter::token_program_for(&wallet.runtime(), &existing.mint).await?;
    inspect_cancel_instruction(
        &wallet,
        &frozen,
        &existing,
        &missing_cosigners,
        &token_program,
    )?;
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

pub(super) async fn reconcile(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable Perps cancel submission."}),
            verified: None,
        });
    };
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    let wallet = adapter.wallet(&record.input)?;
    match wallet
        .runtime()
        .reconcile_submission(submission, record.dispatch_evidence.as_ref())
        .await?
    {
        ReceiptOutcome::ExpiredWithoutInclusion { reason, .. } => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"reason":reason,"settlement_verifier":"expired_without_inclusion_not_supported"}),
            verified: None,
        }),
        ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"confirmed":confirmed,"reason":reason,"request":binding["request"]}),
            verified: None,
        }),
        ReceiptOutcome::NotDispatched {
            reason_code,
            reason,
        } => {
            let verified =
                crate::adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
            Ok(Reconciliation {
                status: Status::Failed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
        ReceiptOutcome::Finalized {
            signature,
            slot,
            fee_lamports,
            failed,
            message_sha256,
            proof,
        } => {
            if failed {
                let verified = crate::adapter::verify_failed_cost(
                    record,
                    submission,
                    &signature,
                    slot,
                    fee_lamports,
                    &proof,
                )?;
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                });
            }
            verify_cancel_proof(binding, &proof, &message_sha256, fee_lamports)?;
            let operation_id = binding["parent_operation_id"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned();
            let original_submission: Submission =
                serde_json::from_value(binding["parent_submission"].clone())
                    .map_err(|_| bad_response())?;
            let mut effects = vec![gofin_domain::FinancialEffect::SubmissionCancellation {
                operation_id,
                submission: original_submission,
            }];
            if proof.account_keys.first().map(String::as_str) == Some(wallet.address())
                && fee_lamports > 0
            {
                effects.push(gofin_domain::FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource_for_wallet(wallet.address()),
                    },
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(fee_lamports),
                    },
                    category: "solana_network_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            let evidence = json!({"cluster":CLUSTER,"request":binding["request"],"position":binding["position"],"signature":signature,"slot":slot,"message_sha256":message_sha256,"close_event":decode_close_event_from_logs(&proof.log_messages,binding["request"].as_str().ok_or_else(bad_response)?,binding["wallet_address"].as_str().ok_or_else(bad_response)?,binding["request_mint"].as_str().ok_or_else(bad_response)?)?,"accounting_owner":"original_create_operation","proof":proof});
            let mut verified = gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence,
                observed_at: Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            Ok(Reconciliation {
                status: Status::Completed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
    }
}

fn verify_cancel_proof(
    binding: &Value,
    proof: &FinalizedReceiptProof,
    message_sha256: &str,
    fee: u64,
) -> Result<()> {
    if proof.failed
        || proof.message_sha256 != message_sha256
        || fee > parse_binding_u64(binding, "maximum_network_fee_lamports")?
    {
        return Err(cancel_receipt_mismatch("finalized message or fee bound"));
    }
    let wallet = binding["wallet_address"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request = binding["request"].as_str().ok_or_else(bad_response)?;
    let event = decode_close_event_from_logs(
        &proof.log_messages,
        request,
        wallet,
        binding["request_mint"].as_str().ok_or_else(bad_response)?,
    )
    .map_err(|_| cancel_receipt_mismatch("bound close event"))?;
    if event["executed"].as_bool() != Some(false)
        || event["amount_atoms"].as_str() != binding["amount_atoms"].as_str()
        || event["request_change"] != binding["request_change"]
        || event["request_type"] != binding["request_type"]
        || event["side"] != binding["side"]
    {
        return Err(cancel_receipt_mismatch("close event fields"));
    }
    let instructions = proof
        .message_instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if instructions.len() != 1
        || instructions[0].data_base64
            != binding["close_instruction_data"]
                .as_str()
                .unwrap_or_default()
        || serde_json::to_value(&instructions[0].accounts).map_err(|_| receipt_mismatch())?
            != binding["close_instruction_accounts"]
    {
        return Err(cancel_receipt_mismatch("pinned close instruction graph"));
    }
    let request_delta = proof
        .lamport_delta(request)
        .map_err(|_| cancel_receipt_mismatch("request account rent delta"))?;
    let request_rent = binding
        .get("request_rent_lamports")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(bad_response)?;
    if request_rent == 0
        || !request_delta.is_negative()
        || request_delta.magnitude().as_str() != request_rent.to_string()
    {
        return Err(cancel_receipt_mismatch("request account rent delta"));
    }
    let owner_token = binding["owner_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request_token = binding["request_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let token_program = binding["token_program"].as_str().ok_or_else(bad_response)?;
    let mint = binding["request_mint"].as_str().ok_or_else(bad_response)?;
    let decimals = binding
        .get("mint_decimals")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(bad_response)?;
    let wallet_delta = proof
        .token_account_delta_verified(owner_token, wallet, mint, token_program, decimals)
        .map_err(|_| cancel_receipt_mismatch("owner token account delta"))?;
    let escrow_delta = proof
        .token_account_delta_verified(request_token, request, mint, token_program, decimals)
        .map_err(|_| cancel_receipt_mismatch("request escrow token account delta"))?;
    let amount = event["amount_atoms"].as_str().ok_or_else(bad_response)?;
    if amount != binding["amount_atoms"].as_str().ok_or_else(bad_response)?
        || wallet_delta.is_negative()
        || wallet_delta.is_zero()
        || wallet_delta.magnitude().as_str() != amount
        || !escrow_delta.is_negative()
        || escrow_delta.magnitude().as_str() != amount
    {
        return Err(cancel_receipt_mismatch("wallet refund and escrow debit"));
    }
    Ok(())
}

fn cancel_receipt_mismatch(check: &'static str) -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_SETTLEMENT_MISMATCH",
        "reconciliation",
        format!("Finalized limit-cancel evidence failed the {check} check."),
        "Keep this operation unresolved and inspect its exact request history; do not submit another cancellation.",
    )
}

pub(super) fn decode_close_event_from_logs(
    logs: &[String],
    request: &str,
    owner: &str,
    mint: &str,
) -> Result<Value> {
    let mut stack: Vec<String> = Vec::new();
    let expected = event_discriminator("ClosePositionRequestEvent");
    let mut matched = None;
    for line in logs {
        // Program-data records are emitted inside the current invocation
        // frame. Handle them before generic `Program ...` invocation/exit
        // lines; otherwise the broad prefix branch below consumes them.
        if !line.starts_with("Program data: ")
            && let Some(rest) = line.strip_prefix("Program ")
        {
            if let Some((program, invocation)) = rest.split_once(" invoke [") {
                let depth = invocation
                    .strip_suffix(']')
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(receipt_mismatch)?;
                if depth == 0 || depth > 32 {
                    return Err(receipt_mismatch());
                }
                stack.truncate(depth - 1);
                stack.push(program.to_owned());
                continue;
            }
            if let Some((program, suffix)) = rest.split_once(' ')
                && (suffix == "success" || suffix.starts_with("failed:"))
            {
                if stack.last().map(String::as_str) == Some(program) {
                    stack.pop();
                } else {
                    return Err(receipt_mismatch());
                }
            }
            continue;
        }
        if stack.last().map(String::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
            continue;
        }
        let Some(encoded) = line.strip_prefix("Program data: ") else {
            continue;
        };
        if encoded.len() > 2048 {
            return Err(receipt_mismatch());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| receipt_mismatch())?;
        if bytes.get(..8) != Some(expected.as_slice()) {
            continue;
        }
        let mut c = BorshCursor::new(bytes.get(8..).ok_or_else(receipt_mismatch)?);
        let entire = c.option_bool()?;
        let executed = c.bool()?;
        let request_change = c.u8()?;
        let request_type = c.u8()?;
        let side = c.u8()?;
        let request_key = c.key()?;
        let owner_key = c.key()?;
        let mint_key = c.key()?;
        let amount = c.u64()?;
        let open_time = i64::from_le_bytes(c.take(8)?.try_into().map_err(|_| receipt_mismatch())?);
        c.finish()?;
        if request_key == request && owner_key == owner && mint_key == mint {
            if matched.is_some() {
                return Err(receipt_mismatch());
            }
            matched = Some(
                json!({"entire_position":entire,"executed":executed,"request_change":request_change,"request_type":request_type,"side":side,"request":request_key,"owner":owner_key,"mint":mint_key,"amount_atoms":amount.to_string(),"open_time":open_time}),
            );
        }
    }
    matched.ok_or_else(receipt_mismatch)
}

pub(super) fn external_cancel_cosigners(
    wallet: &str,
    missing_signers: &[String],
) -> Result<Vec<String>> {
    let cosigners = missing_signers
        .iter()
        .filter(|signer| signer.as_str() != wallet)
        .cloned()
        .collect::<Vec<_>>();
    if cosigners.len() > 1 {
        return Err(unreviewed_detail("unexpected cancel co-signer count"));
    }
    Ok(cosigners)
}

#[cfg(test)]
mod signer_scope_tests {
    use super::*;

    fn candidate() -> (BoundarySpec, PreparedOperation) {
        let submission = Submission {
            kind: "solana_transaction".into(),
            reference: "synthetic-signature".into(),
            data: json!({"message_sha256":"synthetic-hash"}),
        };
        let fixed = json!({
            "operation":OP_LIMIT_CANCEL,"network":NETWORK,"cluster":CLUSTER,
            "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":"wallet-name",
            "wallet_address":"wallet-address","request":"__gofin_plan_recovery_request__",
            "parent_operation_id":"__gofin_plan_recovery_parent__",
            "parent_submission":{"kind":"plan_recovery_parent","reference":"create","data":{"recovery_parent":true}},
            "recovery_parent":true
        });
        let spec = BoundarySpec {
            fixed,
            parameters: json!({"maximum_network_fee_lamports":"15000"}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "synthetic limit recovery permission".into(),
        };
        let prepared = PreparedOperation {
            operation: OP_LIMIT_CANCEL.into(),
            input: json!({"wallet":"wallet-name","request":"request-pda","original_operation_id":"create-id"}),
            binding: json!({
                "operation":OP_LIMIT_CANCEL,"wallet_address":"wallet-address","request":"request-pda",
                "parent_operation_id":"create-id","parent_submission":submission,
                "maximum_network_fee_lamports":"10000"
            }),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![],
            },
            action_deadline: None,
            operation_version: 1,
        };
        (spec, prepared)
    }

    #[test]
    fn pre_gofin_plan_recovery_permission_still_binds_the_exact_wallet_and_request() {
        let (mut spec, candidate) = candidate();
        spec.fixed["request"] = json!("__fin_plan_recovery_request__");
        spec.fixed["parent_operation_id"] = json!("__fin_plan_recovery_parent__");
        validate_permission_candidate(&spec, &candidate).unwrap();
        let mut wrong = candidate.clone();
        wrong.binding["wallet_address"] = json!("different-wallet");
        assert!(validate_permission_candidate(&spec, &wrong).is_err());
    }

    #[test]
    fn named_plan_cancel_permission_accepts_only_bound_create_child_and_fee_cap() {
        let (spec, prepared) = candidate();
        assert!(validate_permission_candidate(&spec, &prepared).is_ok());
        let mut tampered = prepared.clone();
        tampered.binding["wallet_address"] = json!("different-wallet");
        assert!(validate_permission_candidate(&spec, &tampered).is_err());
        let mut tampered = prepared.clone();
        tampered.binding["maximum_network_fee_lamports"] = json!("15001");
        assert!(validate_permission_candidate(&spec, &tampered).is_err());
    }

    #[test]
    fn canonical_parent_aliases_reject_divergent_cancel_binding() {
        let submission = Submission {
            kind: "solana_transaction".into(),
            reference: "synthetic-signature".into(),
            data: json!({"message_sha256":"synthetic-hash"}),
        };
        assert!(
            validate_parent_aliases(&json!({
                "parent_operation_id":"parent","parent_submission":submission,
                "original_operation_id":"parent","original_submission":submission
            }))
            .is_ok()
        );
        assert!(
            validate_parent_aliases(&json!({
                "parent_operation_id":"parent","parent_submission":submission,
                "original_operation_id":"other","original_submission":submission
            }))
            .is_err()
        );
    }

    #[test]
    fn owner_signer_union_is_allowed_only_when_owner_is_fee_payer() {
        assert!(cancel_account_should_sign(
            1, true, "wallet", "wallet", None
        ));
        assert!(!cancel_account_should_sign(
            1,
            true,
            "wallet",
            "sponsor",
            Some("different-keeper")
        ));
        assert!(cancel_account_should_sign(
            1,
            false,
            "wallet",
            "sponsor",
            Some("wallet")
        ));
        assert!(!cancel_account_should_sign(
            2,
            true,
            "token-account",
            "wallet",
            None
        ));
        assert!(cancel_account_should_sign(
            0,
            false,
            "keeper",
            "wallet",
            Some("keeper")
        ));
        assert!(!cancel_account_should_sign(
            0,
            true,
            "program-sentinel",
            "wallet",
            None
        ));
        assert!(!cancel_account_should_write(0, true));
        assert!(cancel_account_should_write(0, false));
        assert!(cancel_account_should_write(1, true));
        assert!(!cancel_account_should_write(6, false));
    }

    #[test]
    fn wallet_slot_is_not_an_external_cosigner() {
        let wallet = "wallet";
        let missing = vec![wallet.to_owned()];
        assert!(
            external_cancel_cosigners(wallet, &missing)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_one_foreign_keeper_slot_is_allowed() {
        let wallet = "wallet";
        let keeper = "keeper";
        assert_eq!(
            external_cancel_cosigners(wallet, &[wallet.into(), keeper.into()]).unwrap(),
            vec![keeper]
        );
        assert!(
            external_cancel_cosigners(wallet, &[wallet.into(), keeper.into(), "api-keeper".into()])
                .is_err()
        );
    }

    #[test]
    fn close_event_is_decoded_only_inside_the_perps_invocation_frame() {
        let request = bs58::encode([7u8; 32]).into_string();
        let owner = bs58::encode([8u8; 32]).into_string();
        let mint = bs58::encode([9u8; 32]).into_string();
        let mut data = event_discriminator("ClosePositionRequestEvent").to_vec();
        data.push(0); // entire position: None
        data.push(0); // executed=false
        data.push(1); // request change
        data.push(1); // request type
        data.push(1); // side
        data.extend_from_slice(&[7u8; 32]);
        data.extend_from_slice(&[8u8; 32]);
        data.extend_from_slice(&[9u8; 32]);
        data.extend_from_slice(&100u64.to_le_bytes());
        data.extend_from_slice(&123i64.to_le_bytes());
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let foreign = gofin_solana::token::TOKEN_PROGRAM_ID;
        let decoded = decode_close_event_from_logs(
            &[
                format!("Program {} invoke [1]", crate::jlp::PERPS_PROGRAM_ID),
                format!("Program {foreign} invoke [2]"),
                // Even byte-perfect event data is not attributable to the
                // Perps program while a foreign CPI owns the current frame.
                format!("Program data: {encoded}"),
                format!("Program {foreign} success"),
                format!("Program {} success", crate::jlp::PERPS_PROGRAM_ID),
            ],
            &request,
            &owner,
            &mint,
        );
        assert!(decoded.is_err());

        let decoded = decode_close_event_from_logs(
            &[
                format!("Program {} invoke [1]", crate::jlp::PERPS_PROGRAM_ID),
                format!("Program {foreign} invoke [2]"),
                format!("Program data: {encoded}"),
                format!("Program {foreign} success"),
                format!("Program data: {encoded}"),
                format!("Program {} success", crate::jlp::PERPS_PROGRAM_ID),
            ],
            &request,
            &owner,
            &mint,
        )
        .unwrap();
        assert_eq!(decoded["executed"], false);
        assert_eq!(decoded["amount_atoms"], "100");

        assert!(
            decode_close_event_from_logs(
                &[format!("Program data: {encoded}")],
                &request,
                &owner,
                &mint,
            )
            .is_err()
        );
    }
}
