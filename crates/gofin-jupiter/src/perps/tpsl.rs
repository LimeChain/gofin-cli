//! Take-profit and stop-loss requests on Jupiter Perps.
//!
//! A TP/SL placement is a pending protocol request, not a completed position
//! change. Its eventual keeper movement is reconciled against the original
//! request identity and the exact position/collateral bounds approved here.

use super::*;
use gofin_domain::PendingRequestPositionBound;
use gofin_domain::product_names::MetadataValueExt;

pub(super) const OP_TPSL_CREATE: &str = "jupiter.perps.tpsl.create";
pub(super) const OP_TPSL_UPDATE: &str = "jupiter.perps.tpsl.update";
pub(super) const OP_TPSL_CANCEL: &str = "jupiter.perps.tpsl.cancel";
pub(super) const EXECUTE_CREATE_ACTION: &str = "create-tpsl";
pub(super) const EXECUTE_UPDATE_ACTION: &str = "update-tpsl";
pub(super) const EXECUTE_CANCEL_ACTION: &str = "cancel-tpsl";

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct TpslBoundaryBounds {
    maximum_size_usd_atoms: String,
    maximum_collateral_usd_atoms: String,
    maximum_protocol_fee_usd_atoms: String,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
}

pub(super) async fn prepare_create(
    adapter: &PerpsAdapter,
    input: &Value,
) -> Result<PreparedOperation> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let position_address = requested_position_address(&wallet, input)?;
    gofin_solana::parse_address(&position_address)?;
    let position = adapter
        .position_by_address(&wallet, &position_address)
        .await?;
    if input
        .get("market")
        .and_then(Value::as_str)
        .is_some_and(|market| market_symbol_for_mint(&position.market_mint) != Some(market))
        || input
            .get("side")
            .and_then(Value::as_str)
            .is_some_and(|side| {
                side != if position.side == gofin_domain::PositionKind::DerivativeLong {
                    "long"
                } else {
                    "short"
                }
            })
    {
        return Err(GoFinError::validation(
            "The selected position does not match the requested market and side.",
        ));
    }
    let (request_type, trigger_above) =
        match (required_text(input, "request_type")?, position.side.clone()) {
            ("tp", gofin_domain::PositionKind::DerivativeLong) => ("tp", true),
            ("sl", gofin_domain::PositionKind::DerivativeLong) => ("sl", false),
            ("tp", gofin_domain::PositionKind::DerivativeShort) => ("tp", false),
            ("sl", gofin_domain::PositionKind::DerivativeShort) => ("sl", true),
            _ => {
                return Err(GoFinError::validation(
                    "The selected Perps position side is invalid.",
                ));
            }
        };
    let trigger_price = required_u64(input, "trigger_price_usd_atoms")?;
    if trigger_price == 0 {
        return Err(GoFinError::validation(
            "The TP/SL trigger price must be positive.",
        ));
    }
    let entire_position = input
        .get("entire_position")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            GoFinError::validation("Choose whether the TP/SL closes the entire position.")
        })?;
    let size = if entire_position {
        position.size_usd_atoms
    } else {
        required_u64(input, "size_usd_atoms")?
    };
    if size == 0 || size > position.size_usd_atoms {
        return Err(GoFinError::validation(
            "The TP/SL size must be positive and no larger than the verified position notional.",
        ));
    }
    let receive_token = required_text(input, "receive_token")?;
    let receive_mint = token_mint_for_symbol(receive_token)?.to_owned();
    let maximum_fee_usd = required_u64(input, "maximum_protocol_fee_usd_atoms")?;
    let maximum_network_fee = required_u64(input, "maximum_network_fee_lamports")?;
    let maximum_account_funding = required_u64(input, "maximum_account_funding_lamports")?;
    if maximum_network_fee == 0 {
        return Err(GoFinError::validation(
            "A positive TPSL network fee cap is required.",
        ));
    }
    let response = adapter
        .api
        .request(
            JupiterHost::Perps,
            Method::POST,
            "/tpsl",
            Some(json!({
                "walletAddress":wallet.address(),
                "positionPubkey":position_address,
                "tpsl":[{
                    "receiveToken":receive_token,
                    "entirePosition":entire_position,
                    "sizeUsdDelta":(!entire_position).then(|| size.to_string()),
                    "triggerPrice":trigger_price.to_string(),
                    "requestType":request_type
                }],
                "transactionType":"instant"
            })),
            RateBucket::Main,
            RequestPriority::Financial,
            DEADLINE,
        )
        .await?;
    if response.get("transactionType").and_then(Value::as_str) != Some("instant")
        || response
            .get("requireKeeperSignature")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(unreviewed_detail(
            "TPSL response is not an instant co-signed transaction",
        ));
    }
    let request = one_response_request(&response)?;
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(bad_response)?;
    let last_valid = parse_u64_text(
        response.get("txMetadata").ok_or_else(bad_response)?,
        "lastValidBlockHeight",
    )?;
    let fee_signer = signer.address();
    let allowed = limit_create::foreign_missing_signers(encoded, fee_signer)?;
    let approved_external_cosigners = limit_create::foreign_required_signers(encoded, fee_signer)?;
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            last_valid,
            maximum_network_fee,
            None,
            &allowed,
        )
        .await?;
    let params = inspect_create_message(
        &wallet,
        &frozen,
        &position,
        &request,
        &receive_mint,
        trigger_price,
        trigger_above,
        entire_position,
        size,
    )
    .await?;
    let token_program = crate::adapter::token_program_for(&wallet.runtime(), &receive_mint).await?;
    let request_ata =
        gofin_solana::associated_token_address(&request, &receive_mint, &token_program)?;
    let auxiliary = crate::adapter::validate_auxiliary_instructions_with_authorities(
        &wallet,
        &frozen,
        &[
            receive_mint.clone(),
            position.market_mint.clone(),
            position.collateral_mint.clone(),
        ],
        None,
        maximum_account_funding,
        Some(crate::jlp::PERPS_PROGRAM_ID),
        &[wallet.address().to_owned(), request.clone()],
    )
    .await?;
    let cpi_rent = limit_create::cpi_account_rent(
        &wallet,
        &request,
        &position_address,
        &position.market_mint,
        &position.collateral_mint,
        if position.side == gofin_domain::PositionKind::DerivativeLong {
            1
        } else {
            2
        },
    )
    .await?;
    let request_rent = cpi_rent.request_lamports;
    // The Position PDA already exists and remains funded throughout TP/SL.
    // Only the temporary request PDA and wallet-paid auxiliary accounts are
    // attributable to this request.
    let wallet_funding_lamports = auxiliary
        .funding_accounts
        .iter()
        .filter(|account| account.payer == wallet.address())
        .try_fold(0u64, |total, account| {
            total.checked_add(account.lamports).ok_or_else(bad_response)
        })?;
    let total_funding = wallet_funding_lamports
        .checked_add(request_rent)
        .ok_or_else(bad_response)?;
    if total_funding > maximum_account_funding {
        return Err(GoFinError::validation(
            "The TPSL request and receive-token account rent exceeds its approved cap.",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;

    let resource = resource_for_wallet(wallet.address());
    let request_resource = ResourceId(format!(
        "solana:mainnet-beta:jupiter-perps-request:{request}"
    ));
    let margin = margin_usd_asset();
    let derivative_asset = crate::pricing::position_usd_asset(&position.market_mint)?;
    let receive_asset = asset(
        &receive_mint,
        crate::adapter::token_decimals(&wallet.runtime(), &receive_mint).await?,
    );
    let network_asset = asset("native", 9);
    let (mut valuation, fee_bound) = crate::adapter::valuation(
        adapter.prices.clone(),
        margin.clone(),
        size,
        network_asset.clone(),
        maximum_network_fee,
        maximum_network_fee,
        wallet.address(),
    )
    .await?;
    let rent_usd = gofin_domain::Quantity::from_u64(total_funding).mul_div_ceil(
        &valuation
            .prices
            .iter()
            .find(|price| price.asset == network_asset)
            .ok_or_else(bad_response)?
            .usd_micros_per_unit,
        &valuation
            .prices
            .iter()
            .find(|price| price.asset == network_asset)
            .ok_or_else(bad_response)?
            .unit_atoms,
    )?;
    valuation.additional_allocation_usd_micros = rent_usd;
    let collateral_name = format!("{position_address}:collateral");
    let fee_cost = CostBound {
        location: AccountingLocation::Position {
            resource: resource.clone(),
            position: collateral_name.clone(),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        },
        amount: Amount {
            asset: margin.clone(),
            atoms: gofin_domain::Quantity::from_u64(maximum_fee_usd),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    // Position anchors store unsigned exposure for both sides, so reducing
    // either side moves the principal toward zero with a negative delta.
    let (derivative_minimum, derivative_maximum) =
        tpsl_position_delta_bounds(&position.side, size, entire_position)?;
    let collateral_min =
        gofin_domain::SignedQuantity::new(format!("-{}", position.collateral_usd_atoms))?;
    let collateral_max = gofin_domain::SignedQuantity::new("0")?;
    let position_anchor = position
        .state(resource.clone())
        .anchor
        .ok_or_else(bad_response)?;
    let protection = gofin_domain::PendingRequestProtection {
        position_anchor,
        position_lock_resource: position_lock_resource(&position_address),
        maximum_reduction: gofin_domain::Quantity::from_u64(size),
        trigger_direction: if trigger_above {
            gofin_domain::PendingRequestTriggerDirection::Above
        } else {
            gofin_domain::PendingRequestTriggerDirection::Below
        },
        trigger_price: Amount {
            asset: margin.clone(),
            atoms: gofin_domain::Quantity::from_u64(trigger_price),
        },
        reduce_only: true,
    };
    let protection_parent =
        protection_parent_binding(adapter, input, &wallet, &protection, None).await?;
    let mut expected = vec![EffectConstraint::PendingRequestLifecycle {
        wallet_resource: resource.clone(),
        request_resource: request_resource.clone(),
        request_id: request.clone(),
        request_kind: format!("jupiter-perps-tpsl-{request_type}"),
        conditional_credits: {
            let mut credits = vec![gofin_domain::PendingRequestCreditBound {
                asset: receive_asset.clone(),
                minimum_credit: gofin_domain::Quantity::zero(),
                maximum_credit: None,
            }];
            if position.rent_lamports > 0 {
                credits.push(gofin_domain::PendingRequestCreditBound {
                    asset: asset("native", 9),
                    minimum_credit: gofin_domain::Quantity::zero(),
                    maximum_credit: Some(gofin_domain::Quantity::from_u64(position.rent_lamports)),
                });
            }
            credits
        },
        conditional_pnl: vec![gofin_domain::PendingRequestPnlBound {
            resource: resource.clone(),
            position: position_address.clone(),
            position_kind: position.side.clone(),
            asset: margin.clone(),
            maximum_loss: gofin_domain::Quantity::from_u64(position.collateral_usd_atoms),
        }],
        protection: Some(protection.clone()),
        conditional_positions: {
            let mut positions = vec![
                PendingRequestPositionBound {
                    resource: resource.clone(),
                    position: position_address.clone(),
                    position_kind: position.side.clone(),
                    asset: derivative_asset.clone(),
                    minimum_delta: derivative_minimum,
                    maximum_delta: derivative_maximum,
                },
                PendingRequestPositionBound {
                    resource: resource.clone(),
                    position: collateral_name.clone(),
                    position_kind: gofin_domain::PositionKind::LockedCollateral,
                    asset: margin.clone(),
                    minimum_delta: collateral_min,
                    maximum_delta: collateral_max,
                },
            ];
            if position.rent_lamports > 0 {
                positions.push(PendingRequestPositionBound {
                    resource: resource.clone(),
                    position: format!("{position_address}:rent"),
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                    asset: asset("native", 9),
                    minimum_delta: gofin_domain::SignedQuantity::new(format!(
                        "-{}",
                        position.rent_lamports
                    ))?,
                    maximum_delta: gofin_domain::SignedQuantity::new("0")?,
                });
            }
            positions
        },
    }];
    let funding_rows = funding_rows(
        &auxiliary,
        wallet.address(),
        &request,
        request_rent,
        maximum_account_funding,
    )?;
    if !funding_rows.is_empty() {
        expected.push(EffectConstraint::AccountFundingLifecycle {
            resource: resource.clone(),
            asset: network_asset.clone(),
            accounts: funding_rows,
        });
    }
    let mut debits = Vec::new();
    if total_funding > 0 {
        debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: network_asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(total_funding),
            },
        });
    }
    let mut binding = json!({
        "operation":OP_TPSL_CREATE,"wallet_address":wallet.address(),"request":request,
        "request_resource":request_resource,"request_type":request_type,
        "request_kind":format!("jupiter-perps-tpsl-{request_type}"),"position":position_address,
        "market":market_symbol_for_mint(&position.market_mint).ok_or_else(unreviewed)?,
        "market_mint":position.market_mint,"collateral_mint":position.collateral_mint,"receive_mint":receive_mint,
        "request_ata":request_ata,"wallet_receive_ata":gofin_solana::associated_token_address(wallet.address(),&receive_mint,&token_program)?,"token_program":token_program,"receive_decimals":receive_asset.decimals,"side":if position.side==gofin_domain::PositionKind::DerivativeLong {1} else {2},
        "size_usd_atoms":size.to_string(),"request_size_usd_delta_atoms":params.size_delta.to_string(),"trigger_price_usd_atoms":trigger_price.to_string(),"trigger_above_threshold":trigger_above,
        "protection":protection,
        "entire_position":entire_position,"counter":params.counter.to_string(),"pre_size_usd_atoms":position.size_usd_atoms.to_string(),
        "pre_collateral_usd_atoms":position.collateral_usd_atoms.to_string(),"request_rent_lamports":request_rent.to_string(),
        "funding_lamports":total_funding.to_string(),"funding_accounts":auxiliary.funding_accounts,"position_rent_lamports":position.rent_lamports.to_string(),
        "created_token_accounts":auxiliary.created_token_accounts.iter().map(|account|json!({"address":account.address,"mint":account.mint,"token_program":account.token_program})).collect::<Vec<_>>(),
        "message_sha256":frozen.message_sha256(),"last_valid_block_height":last_valid,"blockhash":frozen.recent_blockhash(),
        "transaction_base64":encoded,"maximum_network_fee_lamports":maximum_network_fee.to_string(),
        "approved_external_cosigners":approved_external_cosigners,
        "maximum_account_funding_lamports":maximum_account_funding.to_string(),"maximum_protocol_fee_usd_atoms":maximum_fee_usd.to_string()
    });
    binding["network"] = json!(NETWORK);
    binding["cluster"] = json!(CLUSTER);
    binding["program"] = json!(crate::jlp::PERPS_PROGRAM_ID);
    if let Some(parent) = protection_parent {
        binding["__gofin_protection_parent"] = parent;
    }
    let mut maximum_costs = vec![fee_bound, fee_cost];
    maximum_costs.retain(|cost| !cost.amount.atoms.is_zero());
    Ok(PreparedOperation {
        operation: OP_TPSL_CREATE.into(),
        input: input.clone(),
        binding,
        summary: format!(
            "Place a {request_type} trigger at {} for {} of the verified Perps position.",
            format_usd_micros(trigger_price),
            format_usd_micros(size)
        ),
        checks: vec![
            check("Pinned instantCreateTpsl instruction and signer roles"),
            check("Exact wallet, position, request PDA and receive-token ATA"),
            check("Bounded fees, rent and finalized position exposure"),
            check("Transaction simulation"),
        ],
        warnings: vec![
            "The trigger remains a pending keeper request; it is not a completed close.".into(),
            "The protocol fee amount cannot be fixed on-chain for a resting trigger; the approved fee amount is a settlement check, and an overrun remains pending for review.".into(),
        ],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
        valuation: Some(valuation),
        intent: FinancialIntent {
            debits,
            maximum_costs,
            expected,
            locks: vec![
                resource,
                request_resource,
                position_lock_resource(&position_address),
            ],
        },
        action_deadline: None,
            operation_version: 1,
    })
}

async fn protection_parent_binding(
    adapter: &PerpsAdapter,
    input: &Value,
    wallet: &SolanaWalletAdapter,
    candidate: &gofin_domain::PendingRequestProtection,
    exclude_request: Option<&str>,
) -> Result<Option<Value>> {
    let parent_id = input
        .get("protection_parent_operation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let position = &candidate.position_anchor.position;
    let resource = candidate.position_anchor.resource.clone();
    let candidates = adapter.protection_candidates(&resource, position)?;
    let unresolved = candidates
        .into_iter()
        .filter(|record| is_unresolved_protection_candidate(record, input, exclude_request))
        .collect::<Vec<_>>();
    let Some(parent_id) = parent_id else {
        if unresolved.is_empty() {
            return Ok(None);
        }
        return Err(GoFinError::validation(
            "An open TP/SL already protects this position. Select its operation ID to add the opposite trigger.",
        ));
    };
    if unresolved.len() != 1 || unresolved[0].id != parent_id {
        return Err(unreviewed_detail(
            "the selected protection parent is not the sole unresolved request for this position",
        ));
    }
    let parent = adapter.parent_operation(parent_id)?.ok_or_else(|| {
        unreviewed_detail("the selected protection parent is absent from the journal")
    })?;
    let parent_protection = verified_open_protection(&parent, wallet.address(), position)?;
    if !parent_protection.compatible_sibling(candidate) {
        return Err(GoFinError::validation(
            "The second TP/SL must protect the same unchanged position with the opposite trigger direction and a bounded reduction.",
        ));
    }
    let parent_binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
    let request_id = parent_binding["request"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request = adapter
        .position_request_by_address(wallet, request_id)
        .await?;
    if request.request_type != 1
        || request.request_change != 2
        || request.executed
        || request.position != *position
        || request.address != request_id
        || request.mint != parent_binding["receive_mint"]
        || request.size_usd_delta.to_string() != request_size_binding(parent_binding)?.to_string()
    {
        return Err(unreviewed_detail(
            "the selected protection parent no longer has its exact finalized open request",
        ));
    }
    let Some(trigger) = limit_create::effective_request_trigger(
        adapter,
        &parent,
        wallet,
        None,
        OP_TPSL_UPDATE,
        "trigger_price_usd_atoms",
    )
    .await?
    else {
        return Err(unreviewed_detail(
            "reconcile the selected protection parent’s trigger updates before pairing it",
        ));
    };
    if request.trigger_price != Some(trigger)
        || request.trigger_above != parent_binding["trigger_above_threshold"].as_bool()
    {
        return Err(unreviewed_detail(
            "the selected protection parent does not match its finalized trigger amendment chain",
        ));
    }
    let submission = parent.submission.as_ref().ok_or_else(bad_response)?;
    Ok(Some(json!({
        "version":1,
        "parent_operation_id":parent.id,
        "parent_submission":submission,
        "parent_request_id":request_id,
        "shared_locks":[candidate.position_lock_resource]
    })))
}

fn is_unresolved_protection_candidate(
    record: &OperationRecord,
    input: &Value,
    exclude_request: Option<&str>,
) -> bool {
    let current_unsubmitted_prepare = record.input == *input
        && record.status == Status::AwaitingApproval
        && record.submission.is_none()
        && record.effects.is_none();
    !record.status.terminal()
        && !current_unsubmitted_prepare
        && record
            .preview
            .as_ref()
            .is_some_and(|preview| preview.binding["request"].as_str() != exclude_request)
}

fn verified_open_protection(
    parent: &OperationRecord,
    wallet: &str,
    position: &str,
) -> Result<gofin_domain::PendingRequestProtection> {
    let preview = parent.preview.as_ref().ok_or_else(bad_response)?;
    let submission = parent.submission.as_ref().ok_or_else(bad_response)?;
    let effects = parent.effects.as_ref().ok_or_else(|| {
        unreviewed_detail("the protection parent has no finalized open-request proof")
    })?;
    if parent.operation != OP_TPSL_CREATE
        || parent.status != Status::Pending
        || preview.binding["wallet_address"].as_str() != Some(wallet)
        || preview.binding["position"].as_str() != Some(position)
        || effects.submission != *submission
        || gofin_domain::verified_effect_digest(effects)? != effects.proof_digest
    {
        return Err(unreviewed_detail(
            "the protection parent is not a proof-backed open TP/SL on this wallet and position",
        ));
    }
    let bound: gofin_domain::PendingRequestProtection =
        serde_json::from_value(preview.binding["protection"].clone())
            .map_err(|_| bad_response())?;
    let mut open = None;
    for effect in &effects.effects {
        if let gofin_domain::FinancialEffect::PendingRequestProgress {
            wallet_resource,
            request_resource,
            request_id,
            request_kind,
            state: gofin_domain::PendingRequestState::Open,
            protection: Some(protection),
            ..
        } = effect
            && request_id == preview.binding["request"].as_str().unwrap_or_default()
            && request_kind == preview.binding["request_kind"].as_str().unwrap_or_default()
            && wallet_resource == &resource_for_wallet(wallet)
            && request_resource.0.as_str()
                == preview.binding["request_resource"]
                    .as_str()
                    .unwrap_or_default()
            && protection == &bound
            && open.replace(protection.clone()).is_some()
        {
            return Err(receipt_mismatch());
        }
    }
    open.ok_or_else(|| {
        unreviewed_detail("the protection parent has no exact finalized Open progress")
    })
}

pub(super) async fn permission_scope(
    adapter: &PerpsAdapter,
    operation: &str,
    input: &Value,
    bounds_value: &Value,
) -> Result<PlanPermission> {
    match operation {
        OP_TPSL_CREATE => permission_create_scope(adapter, input, bounds_value).await,
        OP_TPSL_UPDATE | OP_TPSL_CANCEL => {
            permission_child_scope(adapter, operation, input, bounds_value).await
        }
        _ => Err(permission_unsupported()),
    }
}

async fn permission_create_scope(
    adapter: &PerpsAdapter,
    input: &Value,
    bounds_value: &Value,
) -> Result<PlanPermission> {
    validate_network(input)?;
    let bounds: TpslBoundaryBounds = serde_json::from_value(bounds_value.clone())
        .map_err(|_| GoFinError::validation("Invalid Jupiter TP/SL boundary limits."))?;
    let max_size = gofin_domain::Quantity::new(&bounds.maximum_size_usd_atoms)?;
    let max_collateral = gofin_domain::Quantity::new(&bounds.maximum_collateral_usd_atoms)?;
    let max_fee = gofin_domain::Quantity::new(&bounds.maximum_protocol_fee_usd_atoms)?;
    if max_size.is_zero()
        || max_collateral.is_zero()
        || max_fee.is_zero()
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_account_funding_lamports == 0
    {
        return Err(GoFinError::validation(
            "TP/SL permissions require finite size, collateral-risk, protocol-fee, network-fee and account-funding caps.",
        ));
    }
    let wallet_name = required_text(input, "wallet")?.to_owned();
    let wallet = adapter.wallet(input)?;
    let request_type = required_text(input, "request_type")?;
    if !matches!(request_type, "tp" | "sl") {
        return Err(GoFinError::validation("Choose tp or sl."));
    }
    let (market, side, position, market_mint, collateral_mint) =
        if let Some(position) = input.get("position").and_then(Value::as_str) {
            gofin_solana::parse_address(position)?;
            let (market, market_mint, collateral_mint, side) =
                derive_position_identity(wallet.address(), position)?;
            (
                market,
                side,
                position.to_owned(),
                market_mint,
                collateral_mint,
            )
        } else {
            let market = required_market_symbol(input, "market")?;
            let side = match required_text(input, "side")? {
                "long" => 1u8,
                "short" => 2u8,
                _ => return Err(GoFinError::validation("Choose long or short.")),
            };
            let market_mint = market_mint_for_symbol(market)?.to_owned();
            let collateral_mint = collateral_mint_for_market(market, side)?.to_owned();
            let position =
                derive_position_address(wallet.address(), &market_mint, &collateral_mint, side)?;
            (market, side, position, market_mint, collateral_mint)
        };
    let request_type = request_type.to_owned();
    let trigger_above = matches!((request_type.as_str(), side), ("tp", 1) | ("sl", 2));
    let trigger = required_u64(input, "trigger_price_usd_atoms")?;
    let receive_token = required_text(input, "receive_token")?;
    let receive_mint = token_mint_for_symbol(receive_token)?.to_owned();
    if !supported_receive_mint(&receive_mint) {
        return Err(GoFinError::validation(
            "Choose a supported TP/SL receive token.",
        ));
    }
    let receive_asset = asset(
        &receive_mint,
        crate::adapter::token_decimals(&wallet.runtime(), &receive_mint).await?,
    );
    let resource = resource_for_wallet(wallet.address());
    let native = asset("native", 9);
    let margin = margin_usd_asset();
    let collateral_position = format!("{position}:collateral");
    let entire_position = input
        .get("entire_position")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            GoFinError::validation("Choose whether the TP/SL covers the entire position.")
        })?;
    let fixed = json!({"operation":OP_TPSL_CREATE,"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":wallet_name,"wallet_address":wallet.address(),"market":market,"market_mint":market_mint,"collateral_mint":collateral_mint,"side":side,"position":position,"request_type":request_type,"trigger_price_usd_atoms":trigger.to_string(),"trigger_above_threshold":trigger_above,"receive_mint":receive_mint,"entire_position":entire_position,"protection_parent_operation_id":input.get("protection_parent_operation_id").cloned().unwrap_or(Value::Null)});
    let spec = BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds).map_err(|_| bad_response())?,
        maximum_debits: vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        }],
        maximum_costs: vec![
            CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: native.clone(),
                    atoms: gofin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
                },
                treatment: CostTreatment::AdditionalDebit,
            },
            CostBound {
                location: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: collateral_position,
                    position_kind: gofin_domain::PositionKind::LockedCollateral,
                },
                amount: Amount {
                    asset: margin.clone(),
                    atoms: max_fee,
                },
                treatment: CostTreatment::AdditionalDebit,
            },
        ],
        summary: format!(
            "Authorize a Jupiter Perps {request_type} for {market} up to ${} notional and ${} collateral exposure, with ${} protocol-fee, {} lamport network-fee and {} lamport account-funding limits. Trigger and receive token remain fixed.",
            format_usd_micros(max_size.as_str().parse().map_err(|_| bad_response())?),
            format_usd_micros(
                max_collateral
                    .as_str()
                    .parse()
                    .map_err(|_| bad_response())?
            ),
            format_usd_micros(
                bounds
                    .maximum_protocol_fee_usd_atoms
                    .parse()
                    .map_err(|_| bad_response())?,
            ),
            bounds.maximum_network_fee_lamports,
            bounds.maximum_account_funding_lamports
        ),
    };
    Ok(PlanPermission {
        recovery_parent: None,
        spec,
        possible_credits: vec![FinancialPlanFundingIdentity {
            resource,
            asset: receive_asset,
        }],
        fee_assets: vec![native, margin],
        deferred_receipt: None,
        authorization_seconds: Some(600),
        cumulative_scope: None,
    })
}

async fn permission_child_scope(
    adapter: &PerpsAdapter,
    operation: &str,
    input: &Value,
    bounds_value: &Value,
) -> Result<PlanPermission> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let wallet_name = required_text(input, "wallet")?.to_owned();
    let recovery_step = bounds_value
        .get("recovery_parent_step_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let supplied_parent = input
        .get("original_operation_id")
        .is_some_and(|value| !value.is_null());
    let request = input
        .get("request")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let (parent_id, parent_submission, request_id, position, recovery_parent) = if let Some(
        step_id,
    ) = recovery_step
    {
        if supplied_parent {
            return Err(GoFinError::validation(
                "For a named-plan TP/SL child, omit the original create ID; GoFin binds it from the preceding create step.",
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
            "__gofin_plan_recovery_position__".to_owned(),
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
        let request = request.ok_or_else(|| {
            GoFinError::validation("Select the exact TP/SL request from GoFin status.")
        })?;
        gofin_solana::parse_address(request)?;
        let parent_id = required_text(input, "original_operation_id")?;
        let parent = adapter.parent_operation(parent_id)?.ok_or_else(|| {
            unreviewed_detail("the original TP/SL create is absent from the local journal")
        })?;
        let submission = parent.submission.clone().ok_or_else(|| {
            unreviewed_detail("the original TP/SL create has no durable submission")
        })?;
        verify_parent_submission_echo(input, &submission)?;
        let binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
        if parent.operation != OP_TPSL_CREATE
            || !matches!(
                parent.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
            || binding["wallet_address"].as_str() != Some(wallet.address())
            || binding["request"].as_str() != Some(request)
        {
            return Err(unreviewed_detail(
                "the original create does not bind this pending TP/SL request and wallet",
            ));
        }
        (
            parent.id,
            submission,
            request.to_owned(),
            binding["position"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
            None,
        )
    };
    let maximum_fee = bounds_value
        .get("maximum_network_fee_lamports")
        .and_then(|value| {
            value
                .as_str()
                .and_then(|text| text.parse::<u64>().ok())
                .or_else(|| value.as_u64())
        })
        .filter(|value| *value > 0)
        .ok_or_else(|| GoFinError::validation("Set a positive maximum network-fee amount."))?;
    let trigger_range = if operation == OP_TPSL_UPDATE {
        let exact = input
            .get("trigger_price_usd_atoms")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0);
        let minimum = bounds_value
            .get("minimum_trigger_price_usd_atoms")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .or(exact)
            .ok_or_else(|| GoFinError::validation("Set a positive minimum TP/SL trigger."))?;
        let maximum = bounds_value
            .get("maximum_trigger_price_usd_atoms")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .or(exact)
            .ok_or_else(|| GoFinError::validation("Set a positive maximum TP/SL trigger."))?;
        if minimum == 0 || maximum < minimum {
            return Err(GoFinError::validation(
                "The TP/SL trigger range is invalid.",
            ));
        }
        Some((minimum, maximum))
    } else {
        None
    };
    let resource = resource_for_wallet(wallet.address());
    let native = asset("native", 9);
    let fee_atoms = gofin_domain::Quantity::from_u64(maximum_fee);
    let mut fixed = json!({
        "operation":operation,"network":NETWORK,"cluster":CLUSTER,
        "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":wallet_name,
        "wallet_address":wallet.address(),"request":request_id,
        "position":position,"parent_operation_id":parent_id,
        "parent_submission":parent_submission,
        "recovery_parent":recovery_step.is_some()
    });
    if let Some((minimum, maximum)) = trigger_range {
        fixed["trigger_price_usd_atoms"] = input
            .get("trigger_price_usd_atoms")
            .cloned()
            .unwrap_or(Value::Null);
        fixed["trigger_range"] = json!({
            "minimum_trigger_price_usd_atoms":minimum.to_string(),
            "maximum_trigger_price_usd_atoms":maximum.to_string()
        });
    }
    let maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: fee_atoms,
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    Ok(PlanPermission {
        recovery_parent,
        spec: BoundarySpec {
            fixed,
            parameters: json!({"maximum_network_fee_lamports":maximum_fee.to_string()}),
            maximum_debits: vec![],
            maximum_costs,
            summary: if operation == OP_TPSL_UPDATE {
                let (minimum, maximum) = trigger_range.ok_or_else(bad_response)?;
                format!(
                    "Authorize this TP/SL request to move its trigger between {} and {}, with network fees up to {maximum_fee} lamports.",
                    format_usd_micros(minimum),
                    format_usd_micros(maximum)
                )
            } else {
                format!(
                    "Authorize cancellation of this exact TP/SL request, with network fees up to {maximum_fee} lamports."
                )
            },
        },
        possible_credits: vec![],
        fee_assets: vec![native],
        deferred_receipt: None,
        authorization_seconds: Some(600),
        cumulative_scope: None,
    })
}

pub(super) async fn prepare_update(
    adapter: &PerpsAdapter,
    input: &Value,
) -> Result<PreparedOperation> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let request_address = required_text(input, "request")?.to_owned();
    gofin_solana::parse_address(&request_address)?;
    let parent_id = required_text(input, "original_operation_id")?.to_owned();
    let parent = adapter.parent_operation(&parent_id)?.ok_or_else(|| {
        unreviewed_detail("the original TP/SL create is absent from the local journal")
    })?;
    let parent_submission = parent
        .submission
        .as_ref()
        .ok_or_else(|| unreviewed_detail("the original TP/SL create has no durable submission"))?
        .clone();
    if parent.operation != OP_TPSL_CREATE
        || parent.status.terminal()
        || !matches!(
            parent.status,
            Status::Submitted | Status::Pending | Status::Unknown
        )
    {
        return Err(unreviewed_detail(
            "the original TP/SL create is not an unresolved request",
        ));
    }
    verify_parent_submission_echo(input, &parent_submission)?;
    let parent_binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
    if parent_binding["request"].as_str() != Some(request_address.as_str())
        || parent_binding["wallet_address"].as_str() != Some(wallet.address())
        || parent_binding["operation"].as_str() != Some(OP_TPSL_CREATE)
    {
        return Err(unreviewed_detail(
            "the saved TP/SL create identity does not match this request and wallet",
        ));
    }
    let existing = adapter
        .position_request_by_address(&wallet, &request_address)
        .await?;
    if existing.request_type != 1 || existing.request_change != 2 || existing.executed {
        return Err(unreviewed_detail(
            "the exact TP/SL request is no longer open and unexecuted",
        ));
    }
    if existing.position != parent_binding["position"]
        || existing.mint != parent_binding["receive_mint"]
        || existing.side.to_string()
            != parent_binding["side"]
                .as_u64()
                .ok_or_else(bad_response)?
                .to_string()
        || existing.size_usd_delta.to_string() != request_size_binding(parent_binding)?.to_string()
    {
        return Err(unreviewed_detail(
            "the request layout differs from its original create",
        ));
    }
    let position = adapter
        .position_by_address(&wallet, &existing.position)
        .await?;
    if position.size_usd_atoms != parse_binding_u64(parent_binding, "pre_size_usd_atoms")?
        || position.collateral_usd_atoms
            != parse_binding_u64(parent_binding, "pre_collateral_usd_atoms")?
    {
        return Err(unreviewed_detail(
            "the protected position changed while its TP/SL request was open",
        ));
    }
    let trigger = required_u64(input, "trigger_price_usd_atoms")?;
    let above = parent_binding["trigger_above_threshold"]
        .as_bool()
        .ok_or_else(bad_response)?;
    if trigger == 0 {
        return Err(GoFinError::validation(
            "The amended trigger price must be positive; its approved above/below direction remains fixed.",
        ));
    }
    let maximum_network_fee = required_u64(input, "maximum_network_fee_lamports")?;
    if maximum_network_fee == 0 {
        return Err(GoFinError::validation(
            "A positive network-fee cap is required.",
        ));
    }
    let response = adapter.api.request(
        JupiterHost::Perps,
        Method::PATCH,
        "/tpsl",
        Some(json!({"positionRequestPubkey":request_address,"triggerPrice":trigger.to_string(),"transactionType":"instant"})),
        RateBucket::Main,
        RequestPriority::Financial,
        DEADLINE,
    ).await?;
    if response.get("transactionType").and_then(Value::as_str) != Some("instant")
        || response
            .get("requireKeeperSignature")
            .and_then(Value::as_bool)
            != Some(true)
        || response
            .get("positionRequestPubkey")
            .and_then(Value::as_str)
            .is_some_and(|key| key != request_address)
    {
        return Err(unreviewed_detail(
            "the TPSL amendment response is not bound to the requested instant update",
        ));
    }
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(bad_response)?;
    let last_valid = parse_u64_text(
        response.get("txMetadata").ok_or_else(bad_response)?,
        "lastValidBlockHeight",
    )?;
    let external = limit_create::foreign_missing_signers(encoded, wallet.address())?;
    let approved_external_cosigners =
        limit_create::foreign_required_signers(encoded, wallet.address())?;
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            last_valid,
            maximum_network_fee,
            None,
            &external,
        )
        .await?;
    inspect_update_message(&wallet, &frozen, &existing, trigger).await?;
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    let resource = resource_for_wallet(wallet.address());
    let request_resource = parent_binding["request_resource"]
        .as_str()
        .ok_or_else(bad_response)?
        .to_owned();
    let position_lock = position_lock_resource(&existing.position);
    let network_asset = asset("native", 9);
    let (valuation, _) = crate::adapter::valuation(
        adapter.prices.clone(),
        network_asset.clone(),
        0,
        network_asset.clone(),
        0,
        maximum_network_fee,
        wallet.address(),
    )
    .await?;
    let mut binding = json!({
        "operation":OP_TPSL_UPDATE,"wallet_address":wallet.address(),"request":request_address,
        "position":existing.position,"position_resource":position_lock,"request_resource":request_resource,
        "size_usd_atoms":existing.size_usd_delta.to_string(),"fee_payer":frozen.fee_payer(),
        "parent_operation_id":parent_id,"parent_submission":parent_submission,
        "__gofin_amendment":{"version":1,"parent_operation_id":parent.id,"parent_submission":parent.submission,"request_id":request_address,"shared_locks":[position_lock.clone(),ResourceId(request_resource.clone())]},
        "previous_trigger_price_usd_atoms":existing.trigger_price.ok_or_else(bad_response)?.to_string(),
        "trigger_price_usd_atoms":trigger.to_string(),"trigger_above_threshold":above,
        "message_sha256":frozen.message_sha256(),"last_valid_block_height":last_valid,
        "transaction_base64":encoded,"maximum_network_fee_lamports":maximum_network_fee.to_string(),
        "approved_external_cosigners":approved_external_cosigners
    });
    binding["original_operation_id"] = json!(parent_id);
    binding["original_submission"] = json!(parent_submission);
    Ok(PreparedOperation {
        operation: OP_TPSL_UPDATE.into(),
        input: input.clone(),
        binding,
        summary: format!(
            "Update the open TP/SL trigger to {}.",
            format_usd_micros(trigger)
        ),
        checks: vec![
            check("Exact parent create, request PDA and unchanged position"),
            check("Pinned instantUpdateTpsl instruction and authorized signer roles"),
        ],
        warnings: vec!["The original TP/SL stays pending until its own keeper or cancellation proof is reconciled.".into()],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
        valuation: Some(valuation),
        intent: FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: network_asset,
                    atoms: gofin_domain::Quantity::from_u64(maximum_network_fee),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![resource, ResourceId(request_resource), position_lock],
        },
        action_deadline: None,
            operation_version: 1,
    })
}

pub(super) async fn prepare_cancel(
    adapter: &PerpsAdapter,
    input: &Value,
) -> Result<PreparedOperation> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let request_id = required_text(input, "request")?.to_owned();
    gofin_solana::parse_address(&request_id)?;
    let parent_id = required_text(input, "original_operation_id")?.to_owned();
    let parent = adapter.parent_operation(&parent_id)?.ok_or_else(|| {
        unreviewed_detail("the original TP/SL create is absent from the local journal")
    })?;
    let parent_submission = parent
        .submission
        .as_ref()
        .ok_or_else(|| unreviewed_detail("the original TP/SL create has no durable submission"))?
        .clone();
    if parent.operation != OP_TPSL_CREATE
        || parent.status.terminal()
        || !matches!(
            parent.status,
            Status::Submitted | Status::Pending | Status::Unknown
        )
    {
        return Err(unreviewed_detail(
            "the original TP/SL create is not an unresolved request",
        ));
    }
    verify_parent_submission_echo(input, &parent_submission)?;
    let parent_binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
    if parent_binding["request"].as_str() != Some(request_id.as_str())
        || parent_binding["wallet_address"].as_str() != Some(wallet.address())
        || parent_binding["operation"].as_str() != Some(OP_TPSL_CREATE)
    {
        return Err(unreviewed_detail(
            "the saved TP/SL create identity does not match this request and wallet",
        ));
    }
    let existing = adapter
        .position_request_by_address(&wallet, &request_id)
        .await?;
    if existing.request_type != 1
        || existing.request_change != 2
        || existing.executed
        || existing.collateral_delta != 0
        || existing.position != parent_binding["position"]
        || existing.mint != parent_binding["receive_mint"]
        || existing.size_usd_delta.to_string() != request_size_binding(parent_binding)?.to_string()
        || existing.side.to_string()
            != parent_binding["side"]
                .as_u64()
                .ok_or_else(bad_response)?
                .to_string()
    {
        return Err(unreviewed_detail(
            "the exact TP/SL request is not open or differs from its saved create binding",
        ));
    }
    let Some(trigger) = limit_create::effective_request_trigger(
        adapter,
        &parent,
        &wallet,
        None,
        OP_TPSL_UPDATE,
        "trigger_price_usd_atoms",
    )
    .await?
    else {
        return Err(unreviewed_detail(
            "a linked TP/SL update is unresolved; reconcile it before cancellation",
        ));
    };
    if existing.trigger_price != Some(trigger) {
        return Err(unreviewed_detail(
            "the open TP/SL request differs from its finalized amendment chain",
        ));
    }
    let maximum_network_fee = required_u64(input, "maximum_network_fee_lamports")?;
    if maximum_network_fee == 0 {
        return Err(GoFinError::validation(
            "A positive network-fee cap is required for TP/SL cancellation.",
        ));
    }
    let response = adapter
        .api
        .request(
            JupiterHost::Perps,
            Method::DELETE,
            "/tpsl",
            Some(json!({"positionRequestPubkey":request_id})),
            RateBucket::Main,
            RequestPriority::Financial,
            DEADLINE,
        )
        .await?;
    if response
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .is_some_and(|returned| returned != request_id)
    {
        return Err(unreviewed_detail(
            "the TP/SL cancel response request identity",
        ));
    }
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(bad_response)?;
    let last_valid = parse_u64_text(
        response.get("txMetadata").ok_or_else(bad_response)?,
        "lastValidBlockHeight",
    )?;
    let missing = gofin_solana::transaction::SolanaRuntime::missing_signers(encoded)?;
    let external = cancel::external_cancel_cosigners(wallet.address(), &missing)?;
    let approved_external_cosigners =
        limit_create::foreign_required_signers(encoded, wallet.address())?;
    cancel::external_cancel_cosigners(wallet.address(), &approved_external_cosigners)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            last_valid,
            maximum_network_fee,
            None,
            &external,
        )
        .await?;
    let token_program =
        crate::adapter::token_program_for(&wallet.runtime(), &existing.mint).await?;
    let instruction =
        cancel::inspect_cancel_instruction(&wallet, &frozen, &existing, &external, &token_program)?;
    if parent_binding["request_ata"].as_str() != Some(instruction.request_token_account.as_str())
        || parent_binding["receive_mint"].as_str() != Some(existing.mint.as_str())
    {
        return Err(unreviewed_detail(
            "the close request graph differs from the original TP/SL account graph",
        ));
    }
    let auxiliary = crate::adapter::validate_auxiliary_instructions_with_primary_program(
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
            "the TP/SL close transaction contains unapproved auxiliary funding",
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
            atoms: gofin_domain::Quantity::from_u64(maximum_network_fee),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let intent_locks = parent
        .preview
        .as_ref()
        .ok_or_else(bad_response)?
        .intent
        .locks
        .clone();
    let mut binding = json!({
        "operation":OP_TPSL_CANCEL,"wallet_address":wallet.address(),"request":request_id,
        "position":existing.position,"position_resource":position_lock_resource(&existing.position),
        "request_resource":parent_binding["request_resource"],"request_mint":existing.mint,
        "receive_mint":existing.mint,"token_program":token_program,
        "owner_token_account":instruction.owner_token_account,"request_token_account":instruction.request_token_account,
        "request_type":existing.request_type,"request_change":2,"side":existing.side,
        "parent_request_change":existing.request_change,
        "amount_atoms":existing.collateral_delta.to_string(),"counter":existing.counter.to_string(),
        "request_rent_lamports":parent_binding["request_rent_lamports"],
        "parent_operation_id":parent.id,"parent_submission":parent_submission,
        "close_instruction_data":instruction.data_base64,"close_instruction_accounts":instruction.accounts,
        "message_sha256":frozen.message_sha256(),"transaction_base64":encoded,
        "last_valid_block_height":last_valid,"maximum_network_fee_lamports":maximum_network_fee.to_string(),
        "approved_external_cosigners":approved_external_cosigners
    });
    binding["original_operation_id"] = json!(parent_id);
    binding["original_submission"] = json!(parent_submission);
    Ok(PreparedOperation {
        operation: OP_TPSL_CANCEL.into(),
        input: input.clone(),
        binding,
        summary: "Cancel the exact open TP/SL request; its create record remains responsible for keeper fills and rent refunds.".into(),
        checks: vec![
            check("Exact unresolved TP/SL create, request and amendment lineage"),
            check("Pinned closePositionRequest3 account graph and signer roles"),
            check("Bounded wallet fee and transaction simulation"),
        ],
        warnings: vec!["Cancellation can race a keeper fill; GoFin reconciles the saved request and finalized child proof before settling the original create.".into()],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
        valuation: Some(crate::adapter::valuation(
            adapter.prices.clone(), asset("native", 9), 0, asset("native", 9), 0,
            maximum_network_fee, wallet.address(),
        ).await?.0),
        intent: FinancialIntent {
            debits: vec![],
            maximum_costs: vec![fee_bound],
            expected: vec![EffectConstraint::SubmissionCancellation {
                operation_id: parent.id,
                submission: parent_submission,
            }],
            locks: intent_locks,
        },
        action_deadline: None,
            operation_version: 1,
    })
}

pub(super) async fn revalidate_cancel(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let parent_id = prepared.binding["parent_operation_id"]
        .as_str()
        .ok_or_else(bad_response)?;
    let parent = adapter
        .parent_operation(parent_id)?
        .ok_or_else(|| unreviewed_detail("the original TP/SL create is unavailable"))?;
    let saved_submission: Submission =
        serde_json::from_value(prepared.binding["parent_submission"].clone())
            .map_err(|_| bad_response())?;
    validate_recovery_parent_aliases(&prepared.binding)?;
    if parent.operation != OP_TPSL_CREATE
        || parent.status.terminal()
        || parent.submission.as_ref() != Some(&saved_submission)
    {
        return Err(unreviewed_detail(
            "the original TP/SL create is no longer pending with the saved submission",
        ));
    }
    let signer = wallet.signer().ok_or_else(unknown)?;
    let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
    if frozen.message_sha256()
        != prepared.binding["message_sha256"]
            .as_str()
            .unwrap_or_default()
        || frozen.last_valid_block_height() != prepared.binding["last_valid_block_height"].as_u64()
    {
        return Err(unreviewed());
    }
    let request = adapter
        .position_request_by_address(
            &wallet,
            prepared.binding["request"]
                .as_str()
                .ok_or_else(bad_response)?,
        )
        .await?;
    if request.executed || request.request_type != 1 || request.request_change != 2 {
        return Err(unreviewed_detail(
            "the TP/SL request executed or changed before cancellation dispatch",
        ));
    }
    let external = super::approved_missing_signers_from_binding(
        prepared.binding["transaction_base64"]
            .as_str()
            .ok_or_else(bad_response)?,
        wallet.address(),
        &prepared.binding,
    )?;
    cancel::external_cancel_cosigners(wallet.address(), &external)?;
    let instruction = cancel::inspect_cancel_instruction(
        &wallet,
        &frozen,
        &request,
        &external,
        prepared.binding["token_program"]
            .as_str()
            .ok_or_else(bad_response)?,
    )?;
    if instruction.request_token_account
        != prepared.binding["request_token_account"]
            .as_str()
            .unwrap_or_default()
        || instruction.owner_token_account
            != prepared.binding["owner_token_account"]
                .as_str()
                .unwrap_or_default()
    {
        return Err(unreviewed_detail(
            "the TP/SL cancellation account graph changed",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

async fn inspect_update_message(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    request: &VerifiedRequest,
    trigger: u64,
) -> Result<()> {
    let instructions = frozen.instructions();
    let perps = instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if perps.len() != 1
        || perps[0].accounts.len() != 12
        || instructions.iter().any(|ix| {
            ix.program_id != crate::jlp::PERPS_PROGRAM_ID
                && ix.program_id != "ComputeBudget111111111111111111111111111111"
        })
    {
        return Err(unreviewed_detail(
            "expected one pinned instantUpdateTpsl instruction",
        ));
    }
    let instruction = perps[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| unreviewed())?;
    let params = decode_update_params(&data)?;
    if params.size_delta != request.size_usd_delta || params.trigger_price != trigger {
        return Err(unreviewed_detail(
            "instantUpdateTpsl size or trigger differs from approval",
        ));
    }
    let custody_data = account_data(wallet, &request.custody).await?;
    let accounts = &instruction.accounts;
    let expected = [
        (3, pda(&[b"perpetuals"])?),
        (4, request.pool.clone()),
        (5, request.position.clone()),
        (6, request.address.clone()),
        (7, request.custody.clone()),
        (8, address_field(&custody_data, 320)?),
        (9, address_field(&custody_data, 106)?),
        (10, pda(&[b"__event_authority"])?),
        (11, crate::jlp::PERPS_PROGRAM_ID.to_owned()),
    ];
    if expected
        .iter()
        .any(|(index, key)| accounts[*index].address != *key)
        || accounts[2].address != wallet.address()
    {
        return Err(unreviewed_detail(
            "instantUpdateTpsl pinned account identities",
        ));
    }
    let required = frozen.required_signers();
    let expected_signers = [
        &accounts[0].address,
        &accounts[1].address,
        &accounts[2].address,
    ];
    if accounts[0].address == wallet.address()
        || accounts[1].address == wallet.address()
        || accounts[0].address == accounts[1].address
        || required.len() != 3
        || required.iter().any(|key| !expected_signers.contains(&key))
        || expected_signers.iter().any(|key| !required.contains(key))
    {
        return Err(unreviewed_detail("instantUpdateTpsl required signer roles"));
    }
    let fee_payer = frozen.fee_payer();
    if !limit_create::compiled_account_privileges_valid(accounts, fee_payer, &[0, 1, 2], &[6]) {
        return Err(unreviewed_detail(
            "instantUpdateTpsl compiled account privileges differ from the pinned IDL and fee payer",
        ));
    }
    Ok(())
}

pub(super) async fn revalidate_update(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let parent = adapter
        .parent_operation(
            prepared.binding["parent_operation_id"]
                .as_str()
                .ok_or_else(bad_response)?,
        )?
        .ok_or_else(|| unreviewed_detail("the original TP/SL create is unavailable"))?;
    let expected_parent_submission: Submission =
        serde_json::from_value(prepared.binding["parent_submission"].clone())
            .map_err(|_| bad_response())?;
    validate_recovery_parent_aliases(&prepared.binding)?;
    if parent.status.terminal()
        || parent
            .submission
            .as_ref()
            .is_none_or(|saved| saved != &expected_parent_submission)
    {
        return Err(unreviewed_detail(
            "the TP/SL create is no longer pending with the saved submission",
        ));
    }
    let request_resource = prepared.binding["request_resource"]
        .as_str()
        .ok_or_else(bad_response)?;
    if prepared.binding.product_value("__gofin_amendment")?["version"].as_u64() != Some(1)
        || prepared.binding.product_value("__gofin_amendment")?["parent_operation_id"].as_str()
            != prepared.binding["parent_operation_id"].as_str()
        || prepared.binding.product_value("__gofin_amendment")?["parent_submission"]
            != serde_json::to_value(&expected_parent_submission).map_err(|_| bad_response())?
        || prepared.binding.product_value("__gofin_amendment")?["request_id"]
            != prepared.binding["request"]
        || prepared.binding.product_value("__gofin_amendment")?["shared_locks"]
            != json!([
                position_lock_resource(
                    prepared.binding["position"]
                        .as_str()
                        .ok_or_else(bad_response)?
                )
                .0,
                request_resource
            ])
    {
        return Err(unreviewed_detail(
            "the TP/SL amendment lock or parent link changed after preparation",
        ));
    }
    let request = adapter
        .position_request_by_address(
            &wallet,
            prepared.binding["request"]
                .as_str()
                .ok_or_else(bad_response)?,
        )
        .await?;
    if request.executed
        || request.trigger_price.map(|value| value.to_string())
            != prepared.binding["previous_trigger_price_usd_atoms"]
                .as_str()
                .map(str::to_owned)
    {
        return Err(unreviewed_detail(
            "the request changed or executed after amendment preparation",
        ));
    }
    let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
    inspect_update_message(
        &wallet,
        &frozen,
        &request,
        parse_binding_u64(&prepared.binding, "trigger_price_usd_atoms")?,
    )
    .await?;
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

pub(super) async fn reconcile_update(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable TPSL update submission exists."}),
            verified: None,
        });
    };
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    let wallet = adapter.wallet(&record.input)?;
    let outcome = wallet
        .runtime()
        .reconcile_submission(submission, record.dispatch_evidence.as_ref())
        .await?;
    let ReceiptOutcome::Finalized {
        signature,
        slot,
        fee_lamports,
        failed,
        message_sha256,
        proof,
    } = outcome
    else {
        return match outcome {
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason}),
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
            _ => unreachable!(),
        };
    };
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
    if proof.signature != signature
        || proof.slot != slot
        || proof.message_sha256 != message_sha256
        || binding["message_sha256"].as_str() != Some(&message_sha256)
    {
        return Err(receipt_mismatch());
    }
    verify_update_receipt(binding, &proof)?;
    let request = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([binding["request"],{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = request.get("value").ok_or_else(bad_response)?;
    if let Some(account) = account.as_object() {
        let data = gofin_solana::token::decode_rpc_account_data(
            account.get("data").ok_or_else(bad_response)?,
        )?;
        let decoded = decode_position_request(
            wallet.address(),
            binding["request"].as_str().ok_or_else(bad_response)?,
            &data,
        )?;
        // The finalized update transaction is authoritative for this child.
        // Current request state may already reflect a later update or keeper fill.
        let _current_request = decoded;
    } else if account.is_null() {
        // A later keeper fill/cancel may close the account before reconciliation.
    } else {
        return Err(bad_response());
    }
    let resource = resource_for_wallet(wallet.address());
    let mut effects = Vec::new();
    if fee_lamports > 0 && binding["fee_payer"].as_str() == Some(wallet.address()) {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: gofin_domain::Quantity::from_u64(fee_lamports),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let evidence = json!({"request":binding["request"],"parent_operation_id":binding["parent_operation_id"],"signature":signature,"slot":slot,"message_sha256":message_sha256,"proof":proof,"trigger_price_usd_atoms":binding["trigger_price_usd_atoms"]});
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

pub(super) async fn reconcile_cancel(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable TP/SL cancellation submission exists."}),
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
            if proof.signature != signature
                || proof.slot != slot
                || proof.message_sha256 != message_sha256
                || binding["message_sha256"].as_str() != Some(message_sha256.as_str())
                || fee_lamports > parse_binding_u64(binding, "maximum_network_fee_lamports")?
            {
                return Err(receipt_mismatch());
            }
            let event = cancel::decode_close_event_from_logs(
                &proof.log_messages,
                binding["request"].as_str().ok_or_else(bad_response)?,
                wallet.address(),
                binding["receive_mint"].as_str().ok_or_else(bad_response)?,
            )?;
            if event["executed"] != false
                || event["request_change"] != 2
                || event["request_type"] != 1
                || event["amount_atoms"] != "0"
                || event["side"] != binding["side"]
            {
                return Err(receipt_mismatch());
            }
            let request = binding["request"].as_str().ok_or_else(bad_response)?;
            let request_delta = proof.lamport_delta(request)?;
            if !request_delta.is_negative()
                || request_delta.magnitude().as_str()
                    != parse_binding_u64(binding, "request_rent_lamports")?.to_string()
            {
                return Err(receipt_mismatch());
            }
            let wallet_paid_fee = proof
                .account_keys
                .first()
                .is_some_and(|key| key == wallet.address());
            let mut effects = vec![gofin_domain::FinancialEffect::SubmissionCancellation {
                operation_id: binding["parent_operation_id"]
                    .as_str()
                    .ok_or_else(bad_response)?
                    .to_owned(),
                submission: serde_json::from_value(binding["parent_submission"].clone())
                    .map_err(|_| bad_response())?,
            }];
            if wallet_paid_fee && fee_lamports > 0 {
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
            let mut verified = gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence: json!({"request":request,"parent_operation_id":binding["parent_operation_id"],"signature":signature,"slot":slot,"message_sha256":message_sha256,"close_event":event,"proof":proof,"accounting_owner":"original_tpsl_create_operation"}),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UpdateTpslParams {
    size_delta: u64,
    trigger_price: u64,
}

fn decode_update_params(data: &[u8]) -> Result<UpdateTpslParams> {
    if data.get(..8) != Some(instruction_discriminator("instantUpdateTpsl").as_slice()) {
        return Err(unreviewed_detail("instantUpdateTpsl discriminator"));
    }
    let mut cursor = BorshCursor::new(data.get(8..).ok_or_else(bad_response)?);
    let params = UpdateTpslParams {
        size_delta: cursor.u64()?,
        trigger_price: cursor.u64()?,
    };
    let request_time = i64::from_le_bytes(cursor.take(8)?.try_into().map_err(|_| bad_response())?);
    if request_time <= 0 {
        return Err(unreviewed_detail("instantUpdateTpsl request time"));
    }
    cursor.finish()?;
    Ok(params)
}

fn verify_update_receipt(binding: &Value, proof: &FinalizedReceiptProof) -> Result<()> {
    let instructions = proof
        .message_instructions
        .iter()
        .filter(|instruction| instruction.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if instructions.len() != 1 || instructions[0].accounts.len() != 12 {
        return Err(receipt_mismatch());
    }
    let instruction = instructions[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| receipt_mismatch())?;
    let params = decode_update_params(&data)?;
    let wallet = binding["wallet_address"]
        .as_str()
        .ok_or_else(bad_response)?;
    let fee_payer = binding["fee_payer"].as_str().ok_or_else(bad_response)?;
    if params.size_delta != parse_binding_u64(binding, "size_usd_atoms")?
        || params.trigger_price != parse_binding_u64(binding, "trigger_price_usd_atoms")?
        || instruction.accounts[2].address != wallet
        || instruction.accounts[5].address != binding["position"]
        || instruction.accounts[6].address != binding["request"]
        || instruction.accounts[3].address != pda(&[b"perpetuals"])?
        || instruction.accounts[10].address != pda(&[b"__event_authority"])?
        || instruction.accounts[11].address != crate::jlp::PERPS_PROGRAM_ID
    {
        return Err(receipt_mismatch());
    }
    if !limit_create::compiled_account_privileges_valid(
        &instruction.accounts,
        fee_payer,
        &[0, 1, 2],
        &[6],
    ) {
        return Err(receipt_mismatch());
    }
    let expected_signers = instruction.accounts[..3]
        .iter()
        .map(|account| account.address.as_str())
        .collect::<Vec<_>>();
    if expected_signers[0] == wallet
        || expected_signers[1] == wallet
        || expected_signers[0] == expected_signers[1]
        || !update_receipt_signers_match(
            &proof.account_keys,
            &proof.required_signatures,
            &proof.signature,
            fee_payer,
            &expected_signers,
        )
    {
        return Err(receipt_mismatch_at(
            "finalized TPSL update signer identities or signature order",
        ));
    }
    Ok(())
}

fn update_receipt_signers_match(
    account_keys: &[String],
    encoded_signatures: &[String],
    transaction_signature: &str,
    fee_payer: &str,
    expected_signers: &[&str],
) -> bool {
    expected_signers.len() == 3
        && encoded_signatures.len() == expected_signers.len()
        && account_keys.len() >= expected_signers.len()
        && account_keys.first().map(String::as_str) == Some(fee_payer)
        && expected_signers.iter().all(|expected| {
            account_keys
                .iter()
                .take(expected_signers.len())
                .filter(|key| key.as_str() == *expected)
                .count()
                == 1
        })
        && encoded_signatures.first().map(String::as_str) == Some(transaction_signature)
}

pub(super) fn validate_permission_candidate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    if prepared.operation == OP_TPSL_UPDATE || prepared.operation == OP_TPSL_CANCEL {
        return validate_child_permission_candidate(spec, prepared);
    }
    if prepared.operation != OP_TPSL_CREATE {
        return Err(permission_unsupported());
    }
    let bounds: TpslBoundaryBounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| GoFinError::validation("Invalid Jupiter TP/SL permission."))?;
    let binding = &prepared.binding;
    for field in [
        "operation",
        "network",
        "cluster",
        "program",
        "wallet_name",
        "wallet_address",
        "market",
        "market_mint",
        "collateral_mint",
        "side",
        "position",
        "request_type",
        "trigger_price_usd_atoms",
        "trigger_above_threshold",
        "receive_mint",
        "entire_position",
        "protection_parent_operation_id",
    ] {
        let candidate = match field {
            "wallet_name" => prepared.input["wallet"].clone(),
            "trigger_price_usd_atoms" => prepared.input[field].clone(),
            "trigger_above_threshold" => binding[field].clone(),
            "entire_position" => prepared.input[field].clone(),
            "protection_parent_operation_id" => {
                prepared.input.get(field).cloned().unwrap_or(Value::Null)
            }
            _ => binding[field].clone(),
        };
        if spec.fixed.get(field) != Some(&candidate) {
            return Err(GoFinError::validation(format!(
                "The TP/SL candidate differs from the approved `{field}` permission field."
            )));
        }
    }
    if parse_binding_u64(binding, "size_usd_atoms")?
        > bounds
            .maximum_size_usd_atoms
            .parse::<u64>()
            .map_err(|_| bad_response())?
        || parse_binding_u64(binding, "pre_collateral_usd_atoms")?
            > bounds
                .maximum_collateral_usd_atoms
                .parse::<u64>()
                .map_err(|_| bad_response())?
        || parse_binding_u64(binding, "maximum_protocol_fee_usd_atoms")?
            > bounds
                .maximum_protocol_fee_usd_atoms
                .parse::<u64>()
                .map_err(|_| bad_response())?
        || parse_binding_u64(binding, "maximum_network_fee_lamports")?
            > bounds.maximum_network_fee_lamports
        || parse_binding_u64(binding, "maximum_account_funding_lamports")?
            > bounds.maximum_account_funding_lamports
    {
        return Err(GoFinError::validation(
            "The TP/SL candidate exceeds its approved size, collateral risk, fee or account-funding bound.",
        ));
    }
    Ok(())
}

fn validate_child_permission_candidate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let fixed = &spec.fixed;
    let binding = &prepared.binding;
    let recovery_mode = fixed.get("recovery_parent").and_then(Value::as_bool) == Some(true);
    for key in [
        "operation",
        "network",
        "cluster",
        "program",
        "wallet_name",
        "wallet_address",
    ] {
        let candidate = match key {
            "operation" => json!(prepared.operation),
            "network" => json!(NETWORK),
            "cluster" => json!(CLUSTER),
            "program" => json!(crate::jlp::PERPS_PROGRAM_ID),
            "wallet_name" => prepared.input.get("wallet").cloned().unwrap_or(Value::Null),
            "wallet_address" => binding
                .get("wallet_address")
                .cloned()
                .unwrap_or(Value::Null),
            _ => unreachable!(),
        };
        if fixed.get(key) != Some(&candidate) {
            return Err(GoFinError::validation(
                "The TP/SL child differs from its approved operation, wallet or network.",
            ));
        }
    }
    if !gofin_domain::product_names::recovery_placeholder(
        fixed.get("request").and_then(Value::as_str),
        "__gofin_plan_recovery_request__",
    ) && fixed.get("request") != binding.get("request")
    {
        return Err(GoFinError::validation(
            "The TP/SL child request differs from the approved request.",
        ));
    }
    if !gofin_domain::product_names::recovery_placeholder(
        fixed.get("position").and_then(Value::as_str),
        "__gofin_plan_recovery_position__",
    ) && fixed.get("position") != binding.get("position")
    {
        return Err(GoFinError::validation(
            "The TP/SL child position differs from the approved request.",
        ));
    }
    if !recovery_mode
        && (fixed.get("parent_operation_id") != binding.get("parent_operation_id")
            || fixed.get("parent_submission") != binding.get("parent_submission"))
    {
        return Err(GoFinError::validation(
            "The TP/SL child is bound to a different create submission.",
        ));
    }
    let maximum_fee = spec.parameters["maximum_network_fee_lamports"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    if parse_binding_u64(binding, "maximum_network_fee_lamports")? > maximum_fee {
        return Err(GoFinError::validation(
            "The TP/SL child exceeds the authorized network-fee cap.",
        ));
    }
    if prepared.operation == OP_TPSL_UPDATE {
        if fixed
            .get("trigger_price_usd_atoms")
            .is_some_and(|value| !value.is_null())
            && fixed.get("trigger_price_usd_atoms") != binding.get("trigger_price_usd_atoms")
        {
            return Err(GoFinError::validation(
                "The TP/SL update differs from its exact approved trigger.",
            ));
        }
        let minimum = fixed["trigger_range"]["minimum_trigger_price_usd_atoms"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let maximum = fixed["trigger_range"]["maximum_trigger_price_usd_atoms"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let trigger = parse_binding_u64(binding, "trigger_price_usd_atoms")?;
        if trigger < minimum || trigger > maximum {
            return Err(GoFinError::validation(
                "The TP/SL trigger falls outside its approved range.",
            ));
        }
    }
    Ok(())
}

fn verify_parent_submission_echo(input: &Value, expected: &Submission) -> Result<()> {
    if let Some(echo) = input
        .get("original_submission")
        .filter(|value| !value.is_null())
    {
        let actual: Submission = serde_json::from_value(echo.clone())
            .map_err(|_| GoFinError::validation("The original submission echo is malformed."))?;
        if &actual != expected {
            return Err(unreviewed_detail(
                "the supplied submission differs from the durable create record",
            ));
        }
    }
    Ok(())
}

fn validate_recovery_parent_aliases(binding: &Value) -> Result<()> {
    let operation_id = binding.get("original_operation_id");
    let submission = binding.get("original_submission");
    if operation_id.is_none() && submission.is_none() {
        // Preserve recovery for records created before these canonical aliases
        // existed; the authoritative parent fields remain required.
        return Ok(());
    }
    let alias_submission: Submission =
        serde_json::from_value(submission.cloned().ok_or_else(bad_response)?)
            .map_err(|_| bad_response())?;
    let parent_submission = binding.get("parent_submission").ok_or_else(bad_response)?;
    if operation_id.and_then(Value::as_str)
        != binding.get("parent_operation_id").and_then(Value::as_str)
        || parent_submission
            != &serde_json::to_value(alias_submission).map_err(|_| bad_response())?
    {
        return Err(unreviewed_detail(
            "the TP/SL recovery-parent aliases differ from the authoritative parent binding",
        ));
    }
    Ok(())
}

fn request_protection(binding: &Value) -> Result<gofin_domain::PendingRequestProtection> {
    serde_json::from_value(
        binding
            .get("protection")
            .cloned()
            .ok_or_else(bad_response)?,
    )
    .map_err(|_| bad_response())
}

pub(super) async fn revalidate(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
    validate_frozen(adapter, prepared, &frozen).await?;
    let position = adapter
        .position_by_address(
            &wallet,
            prepared.binding["position"]
                .as_str()
                .ok_or_else(bad_response)?,
        )
        .await?;
    if position.size_usd_atoms != parse_binding_u64(&prepared.binding, "pre_size_usd_atoms")?
        || position.collateral_usd_atoms
            != parse_binding_u64(&prepared.binding, "pre_collateral_usd_atoms")?
    {
        return Err(unreviewed_detail(
            "the position changed after this TP/SL request was prepared",
        ));
    }
    let protection: gofin_domain::PendingRequestProtection =
        serde_json::from_value(prepared.binding["protection"].clone())
            .map_err(|_| bad_response())?;
    let current_parent = protection_parent_binding(
        adapter,
        &prepared.input,
        &wallet,
        &protection,
        prepared.binding["request"].as_str(),
    )
    .await?;
    if current_parent.as_ref() != prepared.binding.product_get("__gofin_protection_parent")? {
        return Err(unreviewed_detail(
            "the paired TP/SL parent or its finalized open state changed after preparation",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

pub(super) async fn validate_frozen(
    _adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
    frozen: &FrozenTransaction,
) -> Result<()> {
    if frozen.message_sha256()
        != prepared.binding["message_sha256"]
            .as_str()
            .unwrap_or_default()
        || frozen.last_valid_block_height() != prepared.binding["last_valid_block_height"].as_u64()
    {
        return Err(unreviewed());
    }
    let wallet = _adapter.wallet(&prepared.input)?;
    let position = _adapter
        .position_by_address(
            &wallet,
            prepared.binding["position"]
                .as_str()
                .ok_or_else(bad_response)?,
        )
        .await?;
    let params = inspect_create_message(
        &wallet,
        frozen,
        &position,
        prepared.binding["request"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["receive_mint"]
            .as_str()
            .ok_or_else(bad_response)?,
        parse_binding_u64(&prepared.binding, "trigger_price_usd_atoms")?,
        prepared.binding["trigger_above_threshold"]
            .as_bool()
            .ok_or_else(bad_response)?,
        prepared.binding["entire_position"]
            .as_bool()
            .ok_or_else(bad_response)?,
        parse_binding_u64(&prepared.binding, "size_usd_atoms")?,
    )
    .await?;
    if params.counter != parse_binding_u64(&prepared.binding, "counter")?
        || params.request_time <= 0
    {
        return Err(unreviewed());
    }
    Ok(())
}

pub(super) async fn reconcile_create(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable TP/SL placement submission exists."}),
            verified: None,
        });
    };
    let preview = record.preview.as_ref().ok_or_else(bad_response)?;
    let binding = &preview.binding;
    let wallet = adapter.wallet(&record.input)?;
    match wallet
        .runtime()
        .reconcile_submission(submission, record.dispatch_evidence.as_ref())
        .await?
    {
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
            if proof.message_sha256 != message_sha256
                || binding["message_sha256"].as_str() != Some(&message_sha256)
                || fee_lamports > parse_binding_u64(binding, "maximum_network_fee_lamports")?
            {
                return Err(receipt_mismatch());
            }
            let request_id = binding["request"].as_str().ok_or_else(bad_response)?;
            let account = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([request_id,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let value = account.get("value").ok_or_else(bad_response)?;
            let Some(value) = value.as_object() else {
                if let Some(cancelled) = reconcile_cancelled_parent(
                    adapter,
                    record,
                    submission,
                    &wallet,
                    request_id,
                    &signature,
                    slot,
                    &message_sha256,
                    &proof,
                    fee_lamports,
                )
                .await?
                {
                    return Ok(cancelled);
                }
                if let Some(terminal) = reconcile_keeper_history(
                    adapter,
                    record,
                    submission,
                    &wallet,
                    request_id,
                    &signature,
                    slot,
                    &message_sha256,
                    &proof,
                    fee_lamports,
                )
                .await?
                {
                    return Ok(terminal);
                }
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"request":request_id,"reason":"The finalized transaction is known but the request account is not yet available."}),
                    verified: None,
                });
            };
            if value.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID)
                || value.get("lamports").and_then(Value::as_u64)
                    != Some(parse_binding_u64(binding, "request_rent_lamports")?)
            {
                return Err(receipt_mismatch());
            }
            let data = gofin_solana::token::decode_rpc_account_data(
                value.get("data").ok_or_else(bad_response)?,
            )?;
            let request = decode_position_request(wallet.address(), request_id, &data)?;
            if request.executed {
                if let Some(terminal) = reconcile_keeper_history(
                    adapter,
                    record,
                    submission,
                    &wallet,
                    request_id,
                    &signature,
                    slot,
                    &message_sha256,
                    &proof,
                    fee_lamports,
                )
                .await?
                {
                    return Ok(terminal);
                }
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"request":request_id,"reason":"The request has executed; waiting for its exact finalized keeper event and post-position anchor."}),
                    verified: None,
                });
            }
            let Some(effective_trigger) = limit_create::effective_request_trigger(
                adapter,
                record,
                &wallet,
                None,
                OP_TPSL_UPDATE,
                "trigger_price_usd_atoms",
            )
            .await?
            else {
                return Ok(pending_keeper_evidence(
                    request_id,
                    "A linked TP/SL amendment is unresolved or its finalized order is unavailable.",
                ));
            };
            verify_open_request(binding, &request, effective_trigger)?;
            let resource = resource_for_wallet(wallet.address());
            let request_resource = ResourceId(
                binding["request_resource"]
                    .as_str()
                    .ok_or_else(bad_response)?
                    .to_owned(),
            );
            let mut effects = vec![gofin_domain::FinancialEffect::PendingRequestProgress {
                wallet_resource: resource.clone(),
                request_resource,
                request_id: request_id.to_owned(),
                request_kind: binding["request_kind"]
                    .as_str()
                    .ok_or_else(bad_response)?
                    .to_owned(),
                state: gofin_domain::PendingRequestState::Open,
                protection: Some(request_protection(binding)?),
                positions: Vec::new(),
                credits: Vec::new(),
                realized_pnl: Vec::new(),
            }];
            for (position_name, account_address, funded, refund_authority) in
                funding_specs(binding, wallet.address())?
            {
                let delta = proof.lamport_delta(&account_address)?;
                if delta.is_negative() || delta.magnitude() != funded {
                    return Err(receipt_mismatch());
                }
                let anchor = (funded != gofin_domain::Quantity::zero()).then(|| transaction_position_anchor(
                    resource.clone(),
                    position_name.clone(),
                    gofin_domain::PositionKind::RefundableAccountFunding,
                    Amount { asset: asset("native", 9), atoms: funded.clone() },
                    &signature,
                    slot,
                    &message_sha256,
                    json!({"source":"finalized_jupiter_perps_tpsl_account_funding","account":account_address,"refund_authority":refund_authority,"request":request_id}),
                ));
                effects.push(gofin_domain::FinancialEffect::AccountFundingProgress {
                    resource: resource.clone(),
                    position: position_name,
                    account: account_address,
                    asset: asset("native", 9),
                    funded: funded.clone(),
                    returned: gofin_domain::Quantity::zero(),
                    remaining: funded.clone(),
                    state: if funded.is_zero() {
                        gofin_domain::AccountFundingState::Closed
                    } else {
                        gofin_domain::AccountFundingState::Open
                    },
                    anchor,
                });
            }
            if fee_lamports > 0
                && proof
                    .account_keys
                    .first()
                    .is_some_and(|payer| payer == wallet.address())
            {
                effects.push(gofin_domain::FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(fee_lamports),
                    },
                    category: "solana_network_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            let digest = gofin_domain::binding_hash(
                &json!({"signature":signature,"slot":slot,"message_sha256":message_sha256,"proof":proof}),
            )?;
            let mut evidence = json!({"cluster":CLUSTER,"request":request_id,"position":request.position,"signature":signature,"slot":slot,"message_sha256":message_sha256,"request_account":request.read_value(),"proof":proof});
            evidence["settlement_progress"] = limit_create::progress_metadata(
                record,
                request_id,
                [(format!("placement:{signature}"), digest)],
            )?;
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
                status: Status::Pending,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_cancelled_parent(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
    submission: &Submission,
    wallet: &SolanaWalletAdapter,
    request: &str,
    placement_signature: &str,
    placement_slot: u64,
    placement_hash: &str,
    placement_proof: &FinalizedReceiptProof,
    placement_fee: u64,
) -> Result<Option<Reconciliation>> {
    let mut children = adapter
        .cancellation_children(&record.id, submission)?
        .into_iter()
        .filter(|child| child.operation == OP_TPSL_CANCEL)
        .collect::<Vec<_>>();
    if children.len() > 1 {
        return Err(receipt_mismatch());
    }
    let Some(child) = children.pop() else {
        return Ok(None);
    };
    let (Some(child_submission), Some(child_effects), Some(child_preview)) = (
        child.submission.as_ref(),
        child.effects.as_ref(),
        child.preview.as_ref(),
    ) else {
        return Ok(None);
    };
    if child.status != Status::Completed
        || child_effects.submission != *child_submission
        || gofin_domain::verified_effect_digest(child_effects)? != child_effects.proof_digest
    {
        return Ok(None);
    }
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    let child_binding = &child_preview.binding;
    if child_binding["parent_operation_id"].as_str() != Some(record.id.as_str())
        || child_binding["request"].as_str() != Some(request)
        || child_binding["wallet_address"].as_str() != Some(wallet.address())
        || serde_json::from_value::<Submission>(child_binding["parent_submission"].clone()).ok().as_ref() != Some(submission)
        || !child_effects.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::SubmissionCancellation { operation_id, submission: cancelled }
                if operation_id == &record.id && cancelled == submission)) { return Ok(None); }
    let child_evidence = &child_effects.evidence;
    let proof: FinalizedReceiptProof =
        serde_json::from_value(child_evidence["proof"].clone()).map_err(|_| receipt_mismatch())?;
    let signature = child_evidence["signature"]
        .as_str()
        .ok_or_else(bad_response)?;
    let slot = child_evidence["slot"].as_u64().ok_or_else(bad_response)?;
    let hash = child_evidence["message_sha256"]
        .as_str()
        .ok_or_else(bad_response)?;
    let event = cancel::decode_close_event_from_logs(
        &proof.log_messages,
        request,
        wallet.address(),
        binding["receive_mint"].as_str().ok_or_else(bad_response)?,
    )?;
    if proof.signature != signature
        || proof.slot != slot
        || proof.message_sha256 != hash
        || child_submission.data["chain_signature"].as_str() != Some(signature)
        || child_submission.data["message_sha256"].as_str() != Some(hash)
        || event["request"] != request
        || event["owner"] != wallet.address()
        || event["mint"] != binding["receive_mint"]
        || event["executed"] != false
        || event["request_type"] != 1
        || event["request_change"] != 2
        || event["amount_atoms"] != "0"
        || event["side"] != binding["side"]
    {
        return Err(receipt_mismatch());
    }
    let request_delta = proof.lamport_delta(request)?;
    let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
    if !request_delta.is_negative()
        || request_delta.magnitude().as_str() != request_rent.to_string()
    {
        return Ok(None);
    }
    let mut returned = request_rent;
    for (_, account, funded, _) in funding_specs(binding, wallet.address())? {
        if account == request
            || funded.is_zero()
            || !proof.account_keys.iter().any(|key| key == &account)
        {
            continue;
        }
        let delta = proof.lamport_delta(&account)?;
        if delta.is_negative() {
            returned = returned
                .checked_add(
                    delta
                        .magnitude()
                        .as_str()
                        .parse::<u64>()
                        .map_err(|_| receipt_mismatch())?,
                )
                .ok_or_else(receipt_mismatch)?;
        }
    }
    let child_fee = child_effects
        .effects
        .iter()
        .find_map(|effect| match effect {
            gofin_domain::FinancialEffect::Cost {
                amount, category, ..
            } if category == "solana_network_fee" => amount.atoms.as_str().parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(0);
    let paid_fee = if proof
        .account_keys
        .first()
        .is_some_and(|key| key == wallet.address())
    {
        child_fee
    } else {
        0
    };
    let wallet_delta = proof
        .lamport_delta(wallet.address())?
        .as_str()
        .parse::<i128>()
        .map_err(|_| receipt_mismatch())?;
    if wallet_delta != i128::from(returned) - i128::from(paid_fee) {
        return Ok(None);
    }
    let resource = resource_for_wallet(wallet.address());
    let mut effects = vec![gofin_domain::FinancialEffect::PendingRequestProgress {
        wallet_resource: resource.clone(),
        request_resource: ResourceId(
            binding["request_resource"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
        ),
        request_id: request.to_owned(),
        request_kind: binding["request_kind"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned(),
        state: gofin_domain::PendingRequestState::Cancelled,
        protection: Some(request_protection(binding)?),
        positions: Vec::new(),
        credits: Vec::new(),
        realized_pnl: Vec::new(),
    }];
    effects.extend(terminal_funding_progress(
        record,
        binding,
        wallet.address(),
        request,
        placement_signature,
        placement_slot,
        placement_hash,
        placement_proof,
        signature,
        slot,
        hash,
        &proof,
    )?);
    if let Some(previous) = &record.effects {
        effects.extend(
            previous
                .effects
                .iter()
                .filter(|effect| {
                    matches!(effect,
            gofin_domain::FinancialEffect::Cost { category, .. } if category == "solana_network_fee")
                })
                .cloned(),
        );
    } else if placement_fee > 0
        && placement_proof
            .account_keys
            .first()
            .is_some_and(|key| key == wallet.address())
    {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: gofin_domain::Quantity::from_u64(placement_fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let placement_digest = gofin_domain::binding_hash(
        &json!({"signature":placement_signature,"slot":placement_slot,"message_sha256":placement_hash,"proof":placement_proof}),
    )?;
    let cancel_digest = gofin_domain::binding_hash(
        &json!({"signature":signature,"slot":slot,"message_sha256":hash,"proof":proof,"event":event}),
    )?;
    let progress = limit_create::progress_metadata(
        record,
        request,
        [
            (format!("placement:{placement_signature}"), placement_digest),
            (format!("cancel:{signature}"), cancel_digest),
        ],
    )?;
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: json!({"request":request,"request_signature":placement_signature,"request_slot":placement_slot,"request_proof":placement_proof,"cancel_operation_id":child.id,"cancel_signature":signature,"cancel_slot":slot,"cancel_proof":proof,"close_event":event,"settlement_progress":progress}),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(Reconciliation {
        status: Status::Completed,
        evidence: verified.evidence.clone(),
        verified: Some(verified),
    }))
}

// The arguments are the independently verified placement and keeper proofs;
// retaining them as named values makes the evidence boundary explicit.
#[allow(clippy::too_many_arguments)]
async fn reconcile_keeper_history(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
    submission: &Submission,
    wallet: &SolanaWalletAdapter,
    request_id: &str,
    placement_signature: &str,
    placement_slot: u64,
    placement_hash: &str,
    placement_proof: &FinalizedReceiptProof,
    placement_fee: u64,
) -> Result<Option<Reconciliation>> {
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    if placement_proof.message_sha256 != placement_hash
        || placement_proof.signature != placement_signature
        || placement_proof.slot != placement_slot
    {
        return Err(receipt_mismatch());
    }
    let history = wallet
        .runtime()
        .rpc()
        .request(
            "getSignaturesForAddress",
            json!([request_id,{"commitment":"finalized","limit":100}]),
        )
        .await?;
    let entries = history
        .as_array()
        .filter(|entries| entries.len() <= 100)
        .ok_or_else(bad_response)?;
    let mut matched = Vec::new();
    for entry in entries {
        let signature = entry
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let slot = entry
            .get("slot")
            .and_then(Value::as_u64)
            .ok_or_else(bad_response)?;
        if signature == placement_signature || slot < placement_slot {
            continue;
        }
        if slot == placement_slot {
            match finalized_signature_follows(
                wallet,
                placement_slot,
                placement_signature,
                signature,
            )
            .await?
            {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    return Ok(Some(pending_keeper_evidence(
                        request_id,
                        "Same-slot transaction order is not yet provable from finalized block signatures.",
                    )));
                }
            }
        }
        let (keeper_fee, keeper_hash, keeper_proof) = match wallet
            .runtime()
            .observe_finalized_transaction(signature)
            .await?
        {
            ReceiptOutcome::Finalized {
                signature: observed_signature,
                slot: observed_slot,
                failed: false,
                fee_lamports,
                message_sha256,
                proof,
            } if observed_signature == signature
                && observed_slot == slot
                && proof.signature == signature
                && proof.slot == slot
                && proof.message_sha256 == message_sha256 =>
            {
                (fee_lamports, message_sha256, *proof)
            }
            _ => continue,
        };
        let position = binding["position"].as_str().ok_or_else(bad_response)?;
        if keeper_proof.error.is_some()
            || !keeper_instruction_matches(&keeper_proof, request_id, position, false)?
        {
            continue;
        }
        let side = binding["side"].as_u64().ok_or_else(bad_response)? as u8;
        let size = parse_binding_u64(binding, "size_usd_atoms")?;
        let entire_position = binding["entire_position"]
            .as_bool()
            .ok_or_else(bad_response)?;
        let events = keeper_events(&keeper_proof.log_messages)?;
        let events = events
            .into_iter()
            .filter(|event| match event {
                KeeperEvent::Decrease {
                    position: event_position,
                    request,
                    owner,
                    side: event_side,
                    size_delta_usd,
                    request_change,
                    request_type,
                    ..
                } => {
                    event_position == position
                        && request == request_id
                        && owner == wallet.address()
                        && *event_side == side
                        && *size_delta_usd > 0
                        && if entire_position {
                            *size_delta_usd <= size
                        } else {
                            *size_delta_usd == size
                        }
                        && *request_change == 2
                        && *request_type == 1
                }
                _ => false,
            })
            .collect::<Vec<_>>();
        if events.len() > 1 {
            return Ok(None);
        }
        if events.len() == 1 {
            if limit_create::effective_request_trigger(
                adapter,
                record,
                wallet,
                Some((slot, signature)),
                OP_TPSL_UPDATE,
                "trigger_price_usd_atoms",
            )
            .await?
            .is_none()
            {
                return Ok(Some(pending_keeper_evidence(
                    request_id,
                    "A linked TP/SL amendment is unresolved or its finalized order is unavailable.",
                )));
            }
            let current_order = match finalized_position_order(
                wallet,
                submission,
                slot,
                signature,
                &format!("keeper:{signature}"),
            )
            .await?
            {
                Some(order) => order,
                None => {
                    return Ok(Some(pending_keeper_evidence(
                        request_id,
                        "The finalized keeper transaction order is unavailable or ambiguous.",
                    )));
                }
            };
            let predecessor =
                protection_predecessor_state(adapter, record, wallet, &current_order).await?;
            let (pre_size, pre_collateral) = match &predecessor {
                ProtectionPredecessor::Baseline => (
                    parse_binding_u64(binding, "pre_size_usd_atoms")?,
                    parse_binding_u64(binding, "pre_collateral_usd_atoms")?,
                ),
                ProtectionPredecessor::Filled {
                    size, collateral, ..
                } => (*size, *collateral),
                ProtectionPredecessor::Pending(reason) => {
                    return Ok(Some(pending_keeper_evidence(request_id, reason)));
                }
            };
            matched.push((
                signature.to_owned(),
                slot,
                keeper_fee,
                keeper_hash,
                keeper_proof,
                events[0].clone(),
                pre_size,
                pre_collateral,
                predecessor,
                current_order,
            ));
        }
    }
    if matched.len() != 1 {
        return Ok(None);
    }
    let (
        signature,
        slot,
        keeper_fee,
        keeper_hash,
        keeper_proof,
        event,
        pre_size,
        pre_collateral,
        predecessor,
        current_order,
    ) = matched.pop().unwrap();
    let KeeperEvent::Decrease {
        position: event_position,
        transfer_amount_usd: _,
        transfer_token,
        position_size_usd,
        pnl_usd,
        has_profit,
        fee_usd,
        size_delta_usd,
        original_position_collateral_usd,
        position_collateral_usd,
        price,
        ..
    } = &event
    else {
        return Err(receipt_mismatch());
    };
    let side = binding["side"].as_u64().ok_or_else(bad_response)? as u8;
    let size = *size_delta_usd;
    let position = binding["position"].as_str().ok_or_else(bad_response)?;
    let expected_post_size = pre_size.checked_sub(size).ok_or_else(receipt_mismatch)?;
    let Some(effective_trigger) = limit_create::effective_request_trigger(
        adapter,
        record,
        wallet,
        Some((slot, &signature)),
        OP_TPSL_UPDATE,
        "trigger_price_usd_atoms",
    )
    .await?
    else {
        return Ok(Some(pending_keeper_evidence(
            request_id,
            "A linked TP/SL amendment is unresolved or its finalized order is unavailable.",
        )));
    };
    let trigger_above = binding["trigger_above_threshold"]
        .as_bool()
        .ok_or_else(bad_response)?;
    if event_position != binding["position"].as_str().ok_or_else(bad_response)?
        || *position_size_usd != expected_post_size
        || *original_position_collateral_usd != pre_collateral
        || *position_collateral_usd > pre_collateral
        || *price == 0
        || (trigger_above && *price < effective_trigger)
        || (!trigger_above && *price > effective_trigger)
        || fee_breakdown_total(&event)? > *fee_usd
    {
        return Ok(None);
    }
    let receive_mint = binding["receive_mint"].as_str().ok_or_else(bad_response)?;
    let token_program = binding["token_program"].as_str().ok_or_else(bad_response)?;
    let receive_decimals = binding["receive_decimals"]
        .as_u64()
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(bad_response)?;
    let receive_account = binding["wallet_receive_ata"]
        .as_str()
        .ok_or_else(bad_response)?;
    if gofin_solana::associated_token_address(wallet.address(), receive_mint, token_program)?
        != receive_account
    {
        return Err(receipt_mismatch());
    }
    let payout = keeper_proof.token_account_delta_verified(
        receive_account,
        wallet.address(),
        receive_mint,
        token_program,
        receive_decimals,
    )?;
    if payout.is_negative()
        || payout.magnitude()
            != gofin_domain::Quantity::from_u64(transfer_token.ok_or_else(receipt_mismatch)?)
    {
        return Ok(None);
    }
    let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
    let request_refund = keeper_proof.lamport_delta(request_id)?;
    if !request_refund.is_negative()
        || request_refund.magnitude() != gofin_domain::Quantity::from_u64(request_rent)
    {
        return Ok(None);
    }
    let wallet_pays_keeper = keeper_proof
        .account_keys
        .first()
        .is_some_and(|key| key == wallet.address());
    let total_network_fee = placement_fee
        .checked_add(if wallet_pays_keeper { keeper_fee } else { 0 })
        .ok_or_else(receipt_mismatch)?;

    let resource = resource_for_wallet(wallet.address());
    let request_resource = ResourceId(
        binding["request_resource"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned(),
    );
    let derivative_asset = crate::pricing::position_usd_asset(
        binding["market_mint"].as_str().ok_or_else(bad_response)?,
    )?;
    let collateral_asset = margin_usd_asset();
    let derivative_kind = if side == 1 {
        gofin_domain::PositionKind::DerivativeLong
    } else {
        gofin_domain::PositionKind::DerivativeShort
    };
    let derivative_delta = tpsl_position_delta(
        &if side == 1 {
            gofin_domain::PositionKind::DerivativeLong
        } else {
            gofin_domain::PositionKind::DerivativeShort
        },
        size,
    )?;
    let derivative_post = Amount {
        asset: derivative_asset.clone(),
        atoms: gofin_domain::Quantity::from_u64(expected_post_size),
    };
    let derivative_anchor = transaction_position_anchor(
        resource.clone(),
        position.to_owned(),
        derivative_kind.clone(),
        derivative_post,
        &signature,
        slot,
        &keeper_hash,
        json!({"source":"finalized_jupiter_perps_tpsl_keeper_event","event":event,"pre_size_usd_atoms":pre_size}),
    );
    let collateral_delta = i128::from(*position_collateral_usd) - i128::from(pre_collateral);
    let collateral_name = format!(
        "{}:collateral",
        binding["position"].as_str().ok_or_else(bad_response)?
    );
    let collateral_anchor = transaction_position_anchor(
        resource.clone(),
        collateral_name.clone(),
        gofin_domain::PositionKind::LockedCollateral,
        Amount {
            asset: collateral_asset.clone(),
            atoms: gofin_domain::Quantity::from_u64(*position_collateral_usd),
        },
        &signature,
        slot,
        &keeper_hash,
        json!({"source":"finalized_jupiter_perps_tpsl_keeper_event","event":event,"pre_collateral_usd_atoms":pre_collateral}),
    );
    let mut positions = vec![gofin_domain::PendingRequestPositionProgress {
        resource: resource.clone(),
        position: binding["position"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned(),
        position_kind: derivative_kind.clone(),
        asset: derivative_asset.clone(),
        delta: gofin_domain::SignedQuantity::new(derivative_delta.to_string())?,
        anchor: derivative_anchor.clone(),
    }];
    let mut effects = vec![gofin_domain::FinancialEffect::PositionChange {
        resource: resource.clone(),
        position: binding["position"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned(),
        asset: derivative_asset.clone(),
        delta: gofin_domain::SignedQuantity::new(derivative_delta.to_string())?,
        scaled_receipt: None,
        anchor: Some(derivative_anchor.clone()),
    }];
    let realized_pnl_delta = gofin_domain::SignedQuantity::new(if *has_profit {
        pnl_usd.to_string()
    } else {
        format!("-{pnl_usd}")
    })?;
    effects.push(gofin_domain::FinancialEffect::PositionRealizedPnl {
        resource: resource.clone(),
        position: position.to_owned(),
        position_kind: derivative_kind.clone(),
        asset: margin_usd_asset(),
        event_id: signature.clone(),
        delta: realized_pnl_delta.clone(),
        anchor: Box::new(derivative_anchor.clone()),
    });
    if collateral_delta != 0 {
        let delta = gofin_domain::SignedQuantity::new(collateral_delta.to_string())?;
        positions.push(gofin_domain::PendingRequestPositionProgress {
            resource: resource.clone(),
            position: collateral_name.clone(),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
            asset: collateral_asset.clone(),
            delta: delta.clone(),
            anchor: collateral_anchor.clone(),
        });
        effects.push(gofin_domain::FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: collateral_name.clone(),
            asset: collateral_asset.clone(),
            delta,
            scaled_receipt: None,
            anchor: Some(collateral_anchor),
        });
    }
    append_predecessor_markers(&mut effects, &predecessor, &current_order, &resource)?;
    let credit_amount = Amount {
        asset: receive_asset_from_binding(binding)?,
        atoms: payout.magnitude(),
    };
    let collateral_debit = pre_collateral
        .checked_sub(*position_collateral_usd)
        .ok_or_else(receipt_mismatch)?;
    // Provenance links describe an actual position-funded wallet receipt.
    // A total-loss event with no payout must not manufacture a zero-credit
    // cash movement merely to satisfy origin accounting.
    if collateral_debit > 0 && !credit_amount.atoms.is_zero() {
        effects.push(gofin_domain::FinancialEffect::OriginMovement {
            from: AccountingLocation::Position {
                resource: resource.clone(),
                position: collateral_name.clone(),
                position_kind: gofin_domain::PositionKind::LockedCollateral,
            },
            to: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            debit: Amount {
                asset: collateral_asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(collateral_debit),
            },
            credit: credit_amount.clone(),
        });
    }
    if !credit_amount.atoms.is_zero() {
        effects.push(gofin_domain::FinancialEffect::UsableCredit {
            to: resource.clone(),
            amount: credit_amount.clone(),
        });
    }
    effects.push(gofin_domain::FinancialEffect::PendingRequestProgress {
        wallet_resource: resource.clone(),
        request_resource,
        request_id: request_id.to_owned(),
        request_kind: binding["request_kind"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned(),
        state: gofin_domain::PendingRequestState::Filled,
        protection: Some(request_protection(binding)?),
        positions,
        credits: if credit_amount.atoms.is_zero() {
            Vec::new()
        } else {
            vec![gofin_domain::PendingRequestCreditProgress {
                asset: credit_amount.asset.clone(),
                amount: credit_amount.atoms.clone(),
            }]
        },
        realized_pnl: vec![gofin_domain::PendingRequestPnlProgress {
            resource: resource.clone(),
            position: position.to_owned(),
            position_kind: derivative_kind.clone(),
            asset: margin_usd_asset(),
            event_id: signature.clone(),
            delta: realized_pnl_delta,
            anchor: derivative_anchor.clone(),
        }],
    });
    if *fee_usd > 0 {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Position {
                resource: resource.clone(),
                position: collateral_name,
                position_kind: gofin_domain::PositionKind::LockedCollateral,
            },
            amount: Amount {
                asset: collateral_asset,
                atoms: gofin_domain::Quantity::from_u64(*fee_usd),
            },
            category: "jupiter_perps_protocol_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    if placement_fee > 0 {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: gofin_domain::Quantity::from_u64(placement_fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    if wallet_pays_keeper && keeper_fee > 0 {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: gofin_domain::Quantity::from_u64(keeper_fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    effects.extend(terminal_funding_progress(
        record,
        binding,
        wallet.address(),
        request_id,
        placement_signature,
        placement_slot,
        placement_hash,
        placement_proof,
        &signature,
        slot,
        &keeper_hash,
        &keeper_proof,
    )?);
    if expected_post_size == 0 {
        let position_rent = parse_binding_u64(binding, "position_rent_lamports")?;
        if position_rent > 0 {
            let rent_name = format!("{position}:rent");
            let rent_asset = asset("native", 9);
            let rent_amount = gofin_domain::Quantity::from_u64(position_rent);
            let delta = keeper_proof.lamport_delta(position)?;
            if !delta.is_negative() || delta.magnitude() != rent_amount {
                return Ok(None);
            }
            let position_index = keeper_proof
                .account_keys
                .iter()
                .position(|key| key == position)
                .ok_or_else(receipt_mismatch)?;
            if keeper_proof
                .post_balances_lamports
                .get(position_index)
                .copied()
                != Some(0)
            {
                return Ok(None);
            }
            let anchor = transaction_position_anchor(
                resource.clone(),
                rent_name.clone(),
                gofin_domain::PositionKind::RefundableAccountFunding,
                Amount {
                    asset: rent_asset.clone(),
                    atoms: gofin_domain::Quantity::zero(),
                },
                &signature,
                slot,
                &keeper_hash,
                json!({"source":"finalized_jupiter_perps_tpsl_position_rent_refund","position":position,"request":request_id}),
            );
            let rent_delta = gofin_domain::SignedQuantity::new(format!("-{position_rent}"))?;
            let progress_anchor = anchor.clone();
            effects.push(gofin_domain::FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: rent_name.clone(),
                asset: rent_asset.clone(),
                delta: rent_delta.clone(),
                scaled_receipt: None,
                anchor: Some(anchor.clone()),
            });
            effects.push(gofin_domain::FinancialEffect::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: rent_name.clone(),
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                to: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                debit: Amount {
                    asset: rent_asset.clone(),
                    atoms: rent_amount.clone(),
                },
                credit: Amount {
                    asset: rent_asset.clone(),
                    atoms: rent_amount.clone(),
                },
            });
            effects.push(gofin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: rent_asset.clone(),
                    atoms: rent_amount.clone(),
                },
            });
            if let Some(gofin_domain::FinancialEffect::PendingRequestProgress {
                positions,
                credits,
                ..
            }) = effects.iter_mut().find(|effect| {
                matches!(
                    effect,
                    gofin_domain::FinancialEffect::PendingRequestProgress {
                        request_id: progress_request,
                        state: gofin_domain::PendingRequestState::Filled,
                        ..
                    } if progress_request == request_id
                )
            }) {
                positions.push(gofin_domain::PendingRequestPositionProgress {
                    resource: resource.clone(),
                    position: rent_name,
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                    asset: rent_asset.clone(),
                    delta: rent_delta,
                    anchor: progress_anchor,
                });
                credits.push(gofin_domain::PendingRequestCreditProgress {
                    asset: rent_asset,
                    amount: rent_amount,
                });
            } else {
                return Err(receipt_mismatch());
            }
        }
    }
    let mut returned_lamports = 0u64;
    for (_, account, _, _) in funding_specs(binding, wallet.address())? {
        if keeper_proof.account_keys.iter().any(|key| key == &account) {
            let delta = keeper_proof.lamport_delta(&account)?;
            if delta.is_negative() {
                returned_lamports = returned_lamports
                    .checked_add(
                        delta
                            .magnitude()
                            .as_str()
                            .parse::<u64>()
                            .map_err(|_| receipt_mismatch())?,
                    )
                    .ok_or_else(receipt_mismatch)?;
            }
        }
    }
    let position_rent_return = if expected_post_size == 0 {
        parse_binding_u64(binding, "position_rent_lamports")?
    } else {
        0
    };
    returned_lamports = returned_lamports
        .checked_add(position_rent_return)
        .ok_or_else(receipt_mismatch)?;
    let wallet_lamport_delta = keeper_proof
        .lamport_delta(wallet.address())?
        .as_str()
        .parse::<i128>()
        .map_err(|_| receipt_mismatch())?;
    let wallet_fee = if wallet_pays_keeper { keeper_fee } else { 0 };
    if wallet_lamport_delta != i128::from(returned_lamports) - i128::from(wallet_fee) {
        return Ok(None);
    }
    let placement_digest = gofin_domain::binding_hash(
        &json!({"signature":placement_signature,"slot":placement_slot,"message_sha256":placement_hash,"proof":placement_proof}),
    )?;
    let keeper_digest = gofin_domain::binding_hash(
        &json!({"signature":signature,"slot":slot,"message_sha256":keeper_hash,"proof":keeper_proof,"event":event}),
    )?;
    let mut evidence = json!({"cluster":CLUSTER,"request":request_id,"position":binding["position"],"request_signature":placement_signature,"request_slot":placement_slot,"request_proof":placement_proof,"keeper_signature":signature,"keeper_slot":slot,"keeper_proof":keeper_proof,"keeper_event":event,"payout_atoms":payout.magnitude().as_str(),"pnl_usd_atoms":pnl_usd,"has_profit":has_profit,"proved_wallet_network_fee_lamports":total_network_fee});
    evidence["settlement_progress"] = limit_create::progress_metadata(
        record,
        request_id,
        [
            (format!("placement:{placement_signature}"), placement_digest),
            (format!("keeper:{signature}"), keeper_digest),
        ],
    )?;
    append_position_order(&mut evidence["settlement_progress"], current_order.clone())?;
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(Reconciliation {
        status: Status::Completed,
        evidence: verified.evidence.clone(),
        verified: Some(verified),
    }))
}

fn pending_keeper_evidence(request: &str, reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        evidence: json!({"request":request,"reason":reason}),
        verified: None,
    }
}

fn verified_no_fill_protection(record: &OperationRecord, wallet: &str) -> Result<bool> {
    if record.status != Status::Completed {
        return Ok(false);
    }
    let (Some(preview), Some(submission), Some(effects)) = (
        record.preview.as_ref(),
        record.submission.as_ref(),
        record.effects.as_ref(),
    ) else {
        return Ok(false);
    };
    if effects.submission != *submission
        || gofin_domain::verified_effect_digest(effects)? != effects.proof_digest
    {
        return Ok(false);
    }
    let binding = &preview.binding;
    let request = binding["request"].as_str().unwrap_or_default();
    let kind = binding["request_kind"].as_str().unwrap_or_default();
    let protection: gofin_domain::PendingRequestProtection =
        match serde_json::from_value(binding["protection"].clone()) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
    let progress = effects.effects.iter().any(|effect| {
        matches!(effect,
        gofin_domain::FinancialEffect::PendingRequestProgress {
            wallet_resource, request_id, request_kind,
            state: gofin_domain::PendingRequestState::Cancelled,
            protection: Some(actual), positions, credits, realized_pnl, ..
        } if wallet_resource == &resource_for_wallet(wallet)
            && request_id == request && request_kind == kind
            && actual == &protection && positions.is_empty()
            && credits.is_empty() && realized_pnl.is_empty())
    });
    if !progress {
        return Ok(false);
    }
    let Some(proof_value) = effects.evidence.get("cancel_proof") else {
        return Ok(false);
    };
    let proof: FinalizedReceiptProof = match serde_json::from_value(proof_value.clone()) {
        Ok(proof) => proof,
        Err(_) => return Ok(false),
    };
    let mint = binding["receive_mint"].as_str().unwrap_or_default();
    let event =
        match cancel::decode_close_event_from_logs(&proof.log_messages, request, wallet, mint) {
            Ok(event) => event,
            Err(_) => return Ok(false),
        };
    Ok(proof.signature
        == effects.evidence["cancel_signature"]
            .as_str()
            .unwrap_or_default()
        && proof.slot == effects.evidence["cancel_slot"].as_u64().unwrap_or_default()
        && event["executed"] == false
        && event["request_type"] == 1
        && event["request_change"] == 2
        && event["amount_atoms"] == "0")
}

fn append_position_order(
    progress: &mut Value,
    order: gofin_domain::PositionEventOrder,
) -> Result<()> {
    let rows = progress
        .get_mut("position_orders")
        .and_then(Value::as_array_mut)
        .ok_or_else(receipt_mismatch)?;
    let row = serde_json::to_value(order.clone()).map_err(|_| receipt_mismatch())?;
    if rows.iter().any(|existing| {
        existing.get("event_id") == Some(&json!(order.event_id))
            || existing.get("submission") == Some(&json!(order.submission))
    }) {
        if rows.iter().any(|existing| existing == &row) {
            return Ok(());
        }
        return Err(receipt_mismatch());
    }
    rows.push(row);
    Ok(())
}

// The Filled variant retains the complete verified predecessor proof context.
// Boxing fields would add indirection at every validation site without
// reducing serialized or journal size.
#[allow(clippy::large_enum_variant)]
enum ProtectionPredecessor {
    Baseline,
    Filled {
        size: u64,
        collateral: u64,
        operation_id: String,
        submission: Submission,
        proof_digest: String,
        source_order: gofin_domain::PositionEventOrder,
        derivative_anchor: gofin_domain::PositionAnchor,
        collateral_anchor: Option<gofin_domain::PositionAnchor>,
    },
    Pending(&'static str),
}

fn append_predecessor_markers(
    effects: &mut Vec<gofin_domain::FinancialEffect>,
    predecessor: &ProtectionPredecessor,
    current_order: &gofin_domain::PositionEventOrder,
    resource: &ResourceId,
) -> Result<()> {
    let ProtectionPredecessor::Filled {
        operation_id,
        submission,
        proof_digest,
        source_order,
        derivative_anchor,
        collateral_anchor,
        ..
    } = predecessor
    else {
        return Ok(());
    };
    let mut anchors = vec![derivative_anchor];
    if let Some(anchor) = collateral_anchor {
        anchors.push(anchor);
    }
    for anchor in anchors {
        let paired = effects.iter().any(|effect| {
            matches!(effect,
            gofin_domain::FinancialEffect::PositionChange {
                resource: current_resource, position, asset, delta, anchor: Some(current_anchor), ..
            } if current_resource == &anchor.resource
                && current_resource == resource
                && position == &anchor.position
                && asset == &anchor.principal.asset
                && current_anchor.resource == anchor.resource
                && current_anchor.position == anchor.position
                && current_anchor.position_kind == anchor.position_kind
                && current_anchor.principal.asset == anchor.principal.asset
                && delta.as_str().parse::<i128>().is_ok_and(|value| value < 0))
        });
        if !paired {
            continue;
        }
        effects.push(gofin_domain::FinancialEffect::PositionAnchorPredecessor {
            predecessor: Box::new(gofin_domain::PositionAnchorPredecessor {
                source_operation_id: operation_id.clone(),
                source_submission: submission.clone(),
                source_proof_digest: proof_digest.clone(),
                source_anchor: anchor.clone(),
                source_order: source_order.clone(),
                current_order: current_order.clone(),
                resource: anchor.resource.clone(),
                position: anchor.position.clone(),
                position_kind: anchor.position_kind.clone(),
                asset: anchor.principal.asset.clone(),
            }),
        });
    }
    Ok(())
}

/// A paired protection can fill after its sibling has already reduced the
/// shared position. Use that sibling's immutable finalized anchor as the
/// current request's pre-state; never substitute a fresh wallet snapshot.
async fn protection_predecessor_state(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
    wallet: &SolanaWalletAdapter,
    current_order: &gofin_domain::PositionEventOrder,
) -> Result<ProtectionPredecessor> {
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    let linked = if let Some(link) = binding.product_get("__gofin_protection_parent")? {
        Some((
            link.get("parent_operation_id")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
                .to_owned(),
            serde_json::from_value::<Submission>(
                link.get("parent_submission")
                    .cloned()
                    .ok_or_else(bad_response)?,
            )
            .map_err(|_| bad_response())?,
            link.get("parent_request_id")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
                .to_owned(),
            false,
        ))
    } else {
        // The first protection has no parent link. Find its exact opposite
        // sibling only through the durable reverse link written by that
        // sibling; candidate discovery alone grants no authority.
        let own_submission = record.submission.as_ref().ok_or_else(bad_response)?;
        let resource = resource_for_wallet(wallet.address());
        let position = binding["position"].as_str().ok_or_else(bad_response)?;
        let mut children = Vec::new();
        for candidate in adapter
            .protection_candidates(&resource, position)?
            .into_iter()
            .filter(|candidate| candidate.id != record.id)
        {
            let Some(child_preview) = candidate.preview.as_ref() else {
                continue;
            };
            let child_binding = &child_preview.binding;
            let Some(link) = child_binding.product_get("__gofin_protection_parent")? else {
                continue;
            };
            let parent_id = link
                .get("parent_operation_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parent_request = link
                .get("parent_request_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parent_submission = serde_json::from_value::<Submission>(
                link.get("parent_submission")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
            .ok();
            if parent_id != record.id
                || parent_request != binding["request"].as_str().unwrap_or_default()
                || parent_submission.as_ref() != Some(own_submission)
            {
                continue;
            }
            if verified_no_fill_protection(&candidate, wallet.address())? {
                continue;
            }
            let parent_request = parent_request.to_owned();
            let parent_submission = parent_submission.expect("validated above");
            children.push((candidate, parent_request, parent_submission));
        }
        if children.is_empty() {
            return Ok(ProtectionPredecessor::Baseline);
        }
        if children.len() != 1 {
            return Ok(ProtectionPredecessor::Pending(
                "More than one durable TP/SL sibling candidate exists; keeper ordering is ambiguous.",
            ));
        }
        let (child, linked_parent_request, linked_parent_submission) = children.pop().unwrap();
        let child_binding = &child.preview.as_ref().ok_or_else(bad_response)?.binding;
        let child_submission = child.submission.clone().ok_or_else(|| {
            unreviewed_detail("the paired TP/SL sibling has no durable placement submission")
        })?;
        let child_request = child_binding["request"].as_str().ok_or_else(bad_response)?;
        // The reverse-link metadata above identifies this current operation;
        // predecessor inspection must instead use the sibling's own identity.
        if linked_parent_submission != *own_submission
            || linked_parent_request != binding["request"].as_str().unwrap_or_default()
        {
            return Err(unreviewed_detail(
                "the reverse TP/SL link does not bind the exact current request submission",
            ));
        }
        Some((child.id, child_submission, child_request.to_owned(), true))
    };
    let Some((parent_id, parent_submission, parent_request, reverse_link)) = linked else {
        return Ok(ProtectionPredecessor::Baseline);
    };
    let parent = match adapter.parent_operation(&parent_id)? {
        Some(parent) => parent,
        None => {
            return Ok(ProtectionPredecessor::Pending(
                "The paired TP/SL parent record is unavailable; its position effect cannot be reconstructed.",
            ));
        }
    };
    let parent_binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
    let reverse_link_valid = if reverse_link {
        let child_link = parent_binding.product_get("__gofin_protection_parent")?;
        child_link.is_some_and(|link| {
            link["parent_operation_id"].as_str() == Some(record.id.as_str())
                && link["parent_request_id"].as_str() == binding["request"].as_str()
                && serde_json::from_value::<Submission>(link["parent_submission"].clone())
                    .ok()
                    .as_ref()
                    == record.submission.as_ref()
        })
    } else {
        true
    };
    if !reverse_link_valid
        || parent.operation != OP_TPSL_CREATE
        || parent_binding["request"].as_str() != Some(parent_request.as_str())
        || parent_binding["position"].as_str() != binding["position"].as_str()
        || parent.submission.as_ref() != Some(&parent_submission)
    {
        return Err(unreviewed_detail(
            "the paired TP/SL predecessor is not bound to the exact original request and submission",
        ));
    }
    if !matches!(parent.status, Status::Completed) {
        if matches!(parent.status, Status::Pending | Status::Submitted) {
            return Ok(ProtectionPredecessor::Baseline);
        }
        if matches!(parent.status, Status::Unknown) {
            return Ok(ProtectionPredecessor::Pending(
                "The paired TP/SL parent has an unresolved outcome; reconcile it before settling this request.",
            ));
        }
        return Ok(ProtectionPredecessor::Baseline);
    }
    let Some(effects) = parent.effects.as_ref() else {
        return Ok(ProtectionPredecessor::Pending(
            "The completed paired TP/SL parent has no durable verified effects.",
        ));
    };
    if effects.submission != parent_submission
        || gofin_domain::verified_effect_digest(effects)? != effects.proof_digest
    {
        return Err(receipt_mismatch());
    }
    let parent_protection: gofin_domain::PendingRequestProtection =
        serde_json::from_value(parent_binding["protection"].clone()).map_err(|_| bad_response())?;
    let current_protection: gofin_domain::PendingRequestProtection =
        serde_json::from_value(binding["protection"].clone()).map_err(|_| bad_response())?;
    if !parent_protection.compatible_sibling(&current_protection) {
        return Err(unreviewed_detail(
            "the paired TP/SL records do not protect the same approved exposure",
        ));
    }
    let mut filled_progress = None;
    for effect in &effects.effects {
        if let gofin_domain::FinancialEffect::PendingRequestProgress {
            wallet_resource,
            request_id,
            request_kind,
            state: gofin_domain::PendingRequestState::Filled,
            protection: Some(protection),
            positions,
            ..
        } = effect
            && request_id == &parent_request
            && request_kind == parent_binding["request_kind"].as_str().unwrap_or_default()
            && wallet_resource == &resource_for_wallet(wallet.address())
            && protection == &parent_protection
            && filled_progress.replace(positions).is_some()
        {
            return Err(receipt_mismatch());
        }
    }
    let Some(positions) = filled_progress else {
        return Ok(ProtectionPredecessor::Baseline);
    };
    let position = binding["position"].as_str().ok_or_else(bad_response)?;
    let mut derivative = None;
    let mut collateral = None;
    for row in positions {
        if row.position == position {
            derivative = Some(row);
        } else if row.position == format!("{position}:collateral") {
            collateral = Some(row);
        }
    }
    let Some(derivative) = derivative else {
        return Err(receipt_mismatch());
    };
    let derivative_effect = effects.effects.iter().any(|effect| matches!(effect,
        gofin_domain::FinancialEffect::PositionChange { resource, position: name, asset, delta, anchor: Some(anchor), .. }
            if resource == &derivative.resource && name == &derivative.position
                && asset == &derivative.asset && delta == &derivative.delta
                && anchor == &derivative.anchor));
    if !derivative_effect
        || derivative
            .delta
            .as_str()
            .parse::<i128>()
            .ok()
            .is_none_or(|v| v >= 0)
    {
        return Err(receipt_mismatch());
    }
    let size = derivative
        .anchor
        .principal
        .atoms
        .as_str()
        .parse::<u64>()
        .map_err(|_| receipt_mismatch())?;
    let collateral_amount = if let Some(row) = collateral {
        let collateral_effect = effects.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::PositionChange { resource, position: name, asset, delta, anchor: Some(anchor), .. }
                if resource == &row.resource && name == &row.position
                    && asset == &row.asset && delta == &row.delta
                    && anchor == &row.anchor));
        if !collateral_effect
            || row
                .delta
                .as_str()
                .parse::<i128>()
                .ok()
                .is_none_or(|v| v > 0)
        {
            return Err(receipt_mismatch());
        }
        row.anchor
            .principal
            .atoms
            .as_str()
            .parse::<u64>()
            .map_err(|_| receipt_mismatch())?
    } else {
        parse_binding_u64(parent_binding, "pre_collateral_usd_atoms")?
    };
    let sibling_signature = effects.evidence["keeper_signature"]
        .as_str()
        .ok_or_else(bad_response)?;
    let sibling_slot = effects.evidence["keeper_slot"]
        .as_u64()
        .ok_or_else(bad_response)?;
    let sibling_progress = gofin_domain::SettlementProgress::from_evidence(&effects.evidence)?
        .ok_or_else(receipt_mismatch)?;
    let source_order = sibling_progress.position_orders.into_iter().find(|order| {
        order.submission == parent_submission
            && order.event_id == format!("keeper:{sibling_signature}")
            && order.slot == sibling_slot
    });
    let Some(source_order) = source_order else {
        return Ok(ProtectionPredecessor::Pending(
            "The paired TP/SL predecessor has no canonical finalized position-event order.",
        ));
    };
    let sibling_hash = effects.evidence["keeper_proof"]["message_sha256"]
        .as_str()
        .ok_or_else(bad_response)?;
    let sibling_proof: FinalizedReceiptProof =
        serde_json::from_value(effects.evidence["keeper_proof"].clone())
            .map_err(|_| receipt_mismatch())?;
    if sibling_proof.signature != sibling_signature
        || sibling_proof.slot != sibling_slot
        || sibling_proof.message_sha256 != sibling_hash
        || effects.evidence["request"].as_str() != Some(parent_request.as_str())
    {
        return Err(receipt_mismatch());
    }
    if source_order.domain != current_order.domain {
        return Ok(ProtectionPredecessor::Pending(
            "The paired TP/SL fills have different order domains.",
        ));
    }
    let predecessor_is_before = if source_order.slot < current_order.slot {
        true
    } else if source_order.slot > current_order.slot {
        false
    } else {
        source_order.transaction_index < current_order.transaction_index
    };
    let same_slot_ambiguous = source_order.slot == current_order.slot
        && source_order.transaction_index == current_order.transaction_index;
    if same_slot_ambiguous {
        return Ok(ProtectionPredecessor::Pending(
            "The paired TP/SL fills have ambiguous finalized transaction order.",
        ));
    }
    if !predecessor_is_before {
        return Ok(ProtectionPredecessor::Baseline);
    }
    let derivative_anchor = derivative.anchor.clone();
    let collateral_anchor = collateral.map(|row| row.anchor.clone());
    Ok(ProtectionPredecessor::Filled {
        size,
        collateral: collateral_amount,
        operation_id: parent.id,
        submission: parent_submission,
        proof_digest: effects.proof_digest.clone(),
        source_order,
        derivative_anchor,
        collateral_anchor,
    })
}

async fn finalized_position_order(
    wallet: &SolanaWalletAdapter,
    submission: &Submission,
    slot: u64,
    signature: &str,
    event_id: &str,
) -> Result<Option<gofin_domain::PositionEventOrder>> {
    let Some(index) = finalized_signature_index(wallet, slot, signature).await? else {
        return Ok(None);
    };
    Ok(Some(gofin_domain::PositionEventOrder {
        submission: submission.clone(),
        domain: NETWORK.to_owned(),
        slot,
        transaction_index: index,
        event_id: event_id.to_owned(),
    }))
}

async fn finalized_signature_index(
    wallet: &SolanaWalletAdapter,
    slot: u64,
    signature: &str,
) -> Result<Option<u32>> {
    let response = wallet.runtime().rpc().request("getBlock", json!([slot,{"commitment":"finalized","transactionDetails":"signatures","rewards":false,"maxSupportedTransactionVersion":0}])).await?;
    let Some(signatures) = response.get("signatures").and_then(Value::as_array) else {
        return Ok(None);
    };
    let indices = signatures
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            value
                .as_str()
                .map(|signature| (signature.to_owned(), index))
        })
        .collect::<Vec<_>>();
    let matches = indices
        .iter()
        .filter(|(candidate, _)| candidate == signature)
        .map(|(_, index)| *index)
        .collect::<Vec<_>>();
    if matches.len() != 1 || indices.len() != signatures.len() {
        return Ok(None);
    }
    Ok(Some(
        u32::try_from(matches[0]).map_err(|_| receipt_mismatch())?,
    ))
}

async fn finalized_signature_follows(
    wallet: &SolanaWalletAdapter,
    slot: u64,
    before: &str,
    after: &str,
) -> Result<Option<bool>> {
    if before == after {
        return Ok(None);
    }
    let Some(before_index) = finalized_signature_index(wallet, slot, before).await? else {
        return Ok(None);
    };
    let Some(after_index) = finalized_signature_index(wallet, slot, after).await? else {
        return Ok(None);
    };
    Ok(Some(before_index < after_index))
}

fn receive_asset_from_binding(binding: &Value) -> Result<AssetId> {
    Ok(asset(
        binding["receive_mint"].as_str().ok_or_else(bad_response)?,
        binding["receive_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(bad_response)?,
    ))
}

fn requested_position_address(wallet: &SolanaWalletAdapter, input: &Value) -> Result<String> {
    if let Some(position) = input.get("position").and_then(Value::as_str) {
        gofin_solana::parse_address(position)?;
        return Ok(position.to_owned());
    }
    let market = required_market_symbol(input, "market")?;
    let side = match required_text(input, "side")? {
        "long" => 1,
        "short" => 2,
        _ => return Err(GoFinError::validation("Choose long or short.")),
    };
    let market_mint = market_mint_for_symbol(market)?;
    let collateral_mint = collateral_mint_for_market(market, side)?;
    derive_position_address(wallet.address(), market_mint, collateral_mint, side)
}

fn tpsl_position_delta(kind: &gofin_domain::PositionKind, size: u64) -> Result<i128> {
    if size == 0 {
        return Err(receipt_mismatch());
    }
    match kind {
        gofin_domain::PositionKind::DerivativeLong => Ok(-i128::from(size)),
        // PositionAnchor principal is stored as an unsigned exposure for both
        // sides; a reduction therefore moves toward zero with a negative delta.
        gofin_domain::PositionKind::DerivativeShort => Ok(-i128::from(size)),
        _ => Err(receipt_mismatch()),
    }
}

fn tpsl_position_delta_bounds(
    kind: &gofin_domain::PositionKind,
    maximum_reduction: u64,
    allow_partial_reduction: bool,
) -> Result<(gofin_domain::SignedQuantity, gofin_domain::SignedQuantity)> {
    if maximum_reduction == 0 {
        return Err(bad_response());
    }
    let maximum_negative = tpsl_position_delta(kind, maximum_reduction)?;
    let minimum = gofin_domain::SignedQuantity::new(maximum_negative.to_string())?;
    let maximum = if allow_partial_reduction {
        gofin_domain::SignedQuantity::new("-1")?
    } else {
        minimum.clone()
    };
    Ok((minimum, maximum))
}

// Both immutable placement and terminal keeper receipt fields are required to
// derive cumulative refundable-account deltas without trusting snapshots.
#[allow(clippy::too_many_arguments)]
fn terminal_funding_progress(
    record: &OperationRecord,
    binding: &Value,
    wallet: &str,
    request: &str,
    placement_signature: &str,
    placement_slot: u64,
    placement_hash: &str,
    placement_proof: &FinalizedReceiptProof,
    keeper_signature: &str,
    keeper_slot: u64,
    keeper_hash: &str,
    proof: &FinalizedReceiptProof,
) -> Result<Vec<gofin_domain::FinancialEffect>> {
    let resource = resource_for_wallet(wallet);
    let native = asset("native", 9);
    let mut effects = Vec::new();
    let previous = record.effects.as_ref();
    for (position, account, funded, refund_authority) in funding_specs(binding, wallet)? {
        if funded.is_zero() {
            continue;
        }
        let previous_effect = previous.and_then(|verified| verified.effects.iter().find(|effect| matches!(effect,
            gofin_domain::FinancialEffect::AccountFundingProgress{resource:actual,position:actual_position,account:actual_account,asset:actual_asset,..}
                if actual==&resource && actual_position==&position && actual_account==&account && actual_asset==&native
        )));
        let fallback = if previous_effect.is_none() {
            if placement_proof.signature != placement_signature
                || placement_proof.slot != placement_slot
                || placement_proof.message_sha256 != placement_hash
            {
                return Err(receipt_mismatch());
            }
            let placement_delta = placement_proof.lamport_delta(&account)?;
            if placement_delta.is_negative() || placement_delta.magnitude() != funded {
                return Err(receipt_mismatch());
            }
            let account_index = placement_proof
                .account_keys
                .iter()
                .position(|key| key == &account)
                .ok_or_else(receipt_mismatch)?;
            let post = *placement_proof
                .post_balances_lamports
                .get(account_index)
                .ok_or_else(receipt_mismatch)?;
            Some(gofin_domain::FinancialEffect::AccountFundingProgress {
                resource: resource.clone(),
                position: position.clone(),
                account: account.clone(),
                asset: native.clone(),
                funded: funded.clone(),
                returned: gofin_domain::Quantity::zero(),
                remaining: funded.clone(),
                state: gofin_domain::AccountFundingState::Open,
                anchor: Some(transaction_position_anchor(
                    resource.clone(),
                    position.clone(),
                    gofin_domain::PositionKind::RefundableAccountFunding,
                    Amount {
                        asset: native.clone(),
                        atoms: gofin_domain::Quantity::from_u64(post),
                    },
                    placement_signature,
                    placement_slot,
                    placement_hash,
                    json!({"source":"original_finalized_tpsl_placement_funding","account":account,"refund_authority":refund_authority,"request":request}),
                )),
            })
        } else {
            None
        };
        let previous_effect = previous_effect.or(fallback.as_ref());
        let Some(gofin_domain::FinancialEffect::AccountFundingProgress {
            funded: old_funded,
            returned: old_returned,
            remaining: old_remaining,
            ..
        }) = previous_effect
        else {
            return Err(receipt_mismatch());
        };
        if old_funded != &funded {
            return Err(receipt_mismatch());
        }
        if !proof.account_keys.iter().any(|key| key == &account) {
            effects.push(previous_effect.cloned().ok_or_else(receipt_mismatch)?);
            continue;
        }
        let delta = proof.lamport_delta(&account)?;
        if !delta.is_zero() && !delta.is_negative() {
            return Err(receipt_mismatch());
        }
        let returned_delta = if delta.is_negative() {
            delta.magnitude()
        } else {
            gofin_domain::Quantity::zero()
        };
        if returned_delta.integer() > old_remaining.integer() {
            return Err(receipt_mismatch());
        }
        if account == request && returned_delta != *old_remaining {
            return Err(receipt_mismatch());
        }
        let returned = old_returned.checked_add(&returned_delta)?;
        let remaining = old_funded.checked_sub(&returned)?;
        let index = proof
            .account_keys
            .iter()
            .position(|key| key == &account)
            .ok_or_else(receipt_mismatch)?;
        let post = *proof
            .post_balances_lamports
            .get(index)
            .ok_or_else(receipt_mismatch)?;
        let anchor = transaction_position_anchor(
            resource.clone(),
            position.clone(),
            gofin_domain::PositionKind::RefundableAccountFunding,
            Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(post),
            },
            keeper_signature,
            keeper_slot,
            keeper_hash,
            json!({"source":"finalized_jupiter_perps_tpsl_account_funding","account":account,"refund_authority":refund_authority,"request":request}),
        );
        effects.push(gofin_domain::FinancialEffect::AccountFundingProgress {
            resource: resource.clone(),
            position,
            account,
            asset: native.clone(),
            funded: old_funded.clone(),
            returned,
            remaining: remaining.clone(),
            state: if remaining.is_zero() {
                gofin_domain::AccountFundingState::Closed
            } else {
                gofin_domain::AccountFundingState::Open
            },
            anchor: Some(anchor),
        });
    }
    Ok(effects)
}

fn verify_open_request(
    binding: &Value,
    request: &VerifiedRequest,
    effective_trigger: u64,
) -> Result<()> {
    if request.address != binding["request"]
        || request.position != binding["position"]
        || request.mint != binding["receive_mint"]
        || request.request_type != 1
        || request.request_change != 2
        || request.side != binding["side"].as_u64().ok_or_else(bad_response)? as u8
        || request.size_usd_delta != request_size_binding(binding)?
        || !tpsl_request_size_matches(
            request.entire_position == Some(true),
            request.size_usd_delta,
            parse_binding_u64(binding, "size_usd_atoms")?,
        )
        || request.collateral_delta != 0
        || request.trigger_price != Some(effective_trigger)
        || request.trigger_above != binding["trigger_above_threshold"].as_bool()
        || request.entire_position != binding["entire_position"].as_bool()
    {
        return Err(receipt_mismatch());
    }
    Ok(())
}

fn funding_specs(
    binding: &Value,
    wallet: &str,
) -> Result<Vec<(String, String, gofin_domain::Quantity, String)>> {
    let mut specs = Vec::new();
    let request = binding["request"].as_str().ok_or_else(bad_response)?;
    let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
    if request_rent > 0 {
        specs.push((
            format!("{request}:rent"),
            request.to_owned(),
            gofin_domain::Quantity::from_u64(request_rent),
            request.to_owned(),
        ));
    }
    let accounts = binding["funding_accounts"]
        .as_array()
        .ok_or_else(bad_response)?;
    for value in accounts {
        if value["payer"].as_str() != Some(wallet) {
            continue;
        }
        let address = value["address"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned();
        let authority = value["refund_authority"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned();
        if authority != wallet && authority != request {
            return Err(receipt_mismatch());
        }
        let lamports = value["lamports"].as_u64().ok_or_else(bad_response)?;
        specs.push((
            format!("account-funding:{address}"),
            address,
            gofin_domain::Quantity::from_u64(lamports),
            authority,
        ));
    }
    Ok(specs)
}

fn one_response_request(response: &Value) -> Result<String> {
    let pubkeys = response
        .get("tpslPubkeys")
        .and_then(Value::as_array)
        .ok_or_else(bad_response)?;
    let rows = response
        .get("tpslRequests")
        .and_then(Value::as_array)
        .ok_or_else(bad_response)?;
    if pubkeys.len() != 1 || rows.len() != 1 {
        return Err(unreviewed_detail(
            "expected exactly one TP/SL request in provider response",
        ));
    }
    let key = pubkeys[0].as_str().ok_or_else(bad_response)?;
    let row_key = rows[0]
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    if key != row_key || rows[0].get("requestType").and_then(Value::as_str).is_none() {
        return Err(unreviewed_detail(
            "TPSL response request identities disagree",
        ));
    }
    gofin_solana::parse_address(key)?;
    Ok(key.to_owned())
}

fn funding_rows(
    auxiliary: &crate::adapter::AuxiliaryEffects,
    wallet: &str,
    request: &str,
    request_rent: u64,
    maximum: u64,
) -> Result<Vec<gofin_domain::RefundableFundingLimit>> {
    let mut rows = Vec::new();
    if request_rent > 0 {
        rows.push(gofin_domain::RefundableFundingLimit {
            position: format!("{request}:rent"),
            account: request.to_owned(),
            maximum_amount: gofin_domain::Quantity::from_u64(request_rent),
        });
    }
    let mut total = request_rent;
    for account in &auxiliary.funding_accounts {
        if account.payer != wallet
            || (account.refund_authority != wallet && account.refund_authority != request)
        {
            continue;
        }
        total = total
            .checked_add(account.lamports)
            .ok_or_else(bad_response)?;
        rows.push(gofin_domain::RefundableFundingLimit {
            position: format!("account-funding:{}", account.address),
            account: account.address.clone(),
            maximum_amount: gofin_domain::Quantity::from_u64(account.lamports),
        });
    }
    if total > maximum {
        return Err(unreviewed_detail(
            "TPSL request account funding exceeds its approved cap",
        ));
    }
    Ok(rows)
}

// All request-derived values are checked against the pinned instruction graph.
#[allow(clippy::too_many_arguments)]
async fn inspect_create_message(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    position: &VerifiedPosition,
    request: &str,
    receive_mint: &str,
    trigger_price: u64,
    trigger_above: bool,
    entire_position: bool,
    size: u64,
) -> Result<CreateTpslParams> {
    let perps = frozen
        .instructions()
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if perps.len() != 1 || perps[0].accounts.len() != 20 {
        return Err(unreviewed_detail(
            "expected one pinned instantCreateTpsl instruction",
        ));
    }
    let instruction = &perps[0];
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| unreviewed_detail("TPSL instruction data is not canonical base64"))?;
    let params = decode_create_params(&bytes)?;
    if params.collateral_delta != 0
        || !tpsl_request_size_matches(entire_position, params.size_delta, size)
        || params.trigger_price != trigger_price
        || params.trigger_above != trigger_above
        || params.entire_position != entire_position
    {
        return Err(unreviewed_detail(
            "TPSL instruction parameters differ from the approved request",
        ));
    }
    let a = &instruction.accounts;
    let market = position.market_mint.as_str();
    let pool = main_pool_address(
        &crate::jlp::PERPS_PROGRAM_ID
            .parse()
            .map_err(|_| invalid_account_set())?,
    )
    .to_string();
    let collateral = limit_create::derive_custody_address(&pool, &position.collateral_mint)?;
    let market_custody = limit_create::derive_custody_address(&pool, market)?;
    let desired_program =
        crate::adapter::token_program_for(&wallet.runtime(), receive_mint).await?;
    let request_ata =
        gofin_solana::associated_token_address(request, receive_mint, &desired_program)?;
    let receiving_ata =
        gofin_solana::associated_token_address(wallet.address(), receive_mint, &desired_program)?;
    let expected = [
        None,
        None,
        Some(wallet.address().to_owned()),
        Some(receiving_ata),
        Some(pda(&[b"perpetuals"])?),
        Some(pool.clone()),
        Some(position.address.clone()),
        Some(request.to_owned()),
        Some(request_ata),
        Some(market_custody.clone()),
        None,
        None,
        Some(collateral.clone()),
        Some(receive_mint.to_owned()),
        Some(crate::jlp::PERPS_PROGRAM_ID.to_owned()),
        Some(desired_program),
        Some("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".into()),
        Some("11111111111111111111111111111111".into()),
        Some(pda(&[b"__event_authority"])?),
        Some(crate::jlp::PERPS_PROGRAM_ID.to_owned()),
    ];
    if a.iter()
        .zip(expected)
        .any(|(account, expected)| expected.is_some_and(|key| account.address != key))
        || a[14].address != crate::jlp::PERPS_PROGRAM_ID
        || !a[0].signer
        || !a[1].signer
        || !a[2].signer
    {
        return Err(unreviewed_detail(
            "TPSL signer role or pinned account identity mismatch",
        ));
    }
    if position_request_pda(&position.address, params.counter, 2)? != request {
        return Err(unreviewed_detail(
            "TPSL request PDA does not match its pinned counter",
        ));
    }
    let pool_data = account_data(wallet, &pool).await?;
    let (pool_name, _) = decode_pool_name(&pool_data)?;
    if pool_name.as_bytes() != MAIN_POOL_NAME
        || limit_create::pool_name_address(&pool_name)? != pool
    {
        return Err(invalid_account_set());
    }
    let market_data = account_data(wallet, &market_custody).await?;
    let collateral_data = account_data(wallet, &collateral).await?;
    if address_field(&market_data, 8)? != pool
        || address_field(&collateral_data, 8)? != pool
        || address_field(&market_data, 40)? != market
        || address_field(&collateral_data, 40)? != position.collateral_mint
        || address_field(&market_data, 106)? != a[11].address
        || address_field(&market_data, 320)? != a[10].address
        || a[10].signer
        || a[10].writable
        || a[11].signer
        || a[11].writable
        || a[14].signer
        || a[14].writable
    {
        return Err(invalid_account_set());
    }
    let required = frozen.required_signers();
    if !limit_create::limit_create_signer_roles_valid(required, a, wallet.address()) {
        return Err(unreviewed_detail(
            "TPSL required signer set differs from pinned keeper roles",
        ));
    }
    if !limit_create::compiled_privileges_valid(a, frozen.fee_payer(), &[2, 3, 5, 7, 8]) {
        return Err(unreviewed_detail(
            "TPSL compiled account privileges differ from pinned IDL and exact fee payer",
        ));
    }
    Ok(params)
}

fn tpsl_request_size_matches(
    entire_position: bool,
    encoded_size: u64,
    effective_size: u64,
) -> bool {
    encoded_size == effective_size || (entire_position && encoded_size == 0)
}

fn request_size_binding(binding: &Value) -> Result<u64> {
    if binding
        .get("request_size_usd_delta_atoms")
        .and_then(Value::as_str)
        .is_some()
    {
        parse_binding_u64(binding, "request_size_usd_delta_atoms")
    } else {
        // Older durable requests stored only the effective size, which was
        // also their encoded PositionRequest size.
        parse_binding_u64(binding, "size_usd_atoms")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CreateTpslParams {
    collateral_delta: u64,
    size_delta: u64,
    trigger_price: u64,
    trigger_above: bool,
    entire_position: bool,
    counter: u64,
    request_time: i64,
}

fn decode_create_params(data: &[u8]) -> Result<CreateTpslParams> {
    if data.get(..8) != Some(instruction_discriminator("instantCreateTpsl").as_slice())
        || data.len() != 8 + 8 + 8 + 8 + 1 + 1 + 8 + 8
    {
        return Err(unreviewed_detail(
            "invalid pinned TPSL create instruction data",
        ));
    }
    let mut offset: usize = 8;
    fn u64_at(data: &[u8], offset: &mut usize) -> Result<u64> {
        let end = offset.checked_add(8).ok_or_else(bad_response)?;
        let bytes = data.get(*offset..end).ok_or_else(bad_response)?;
        *offset = end;
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| bad_response())?,
        ))
    }
    let collateral_delta = u64_at(data, &mut offset)?;
    let size_delta = u64_at(data, &mut offset)?;
    let trigger_price = u64_at(data, &mut offset)?;
    let trigger_above = match data.get(offset) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(unreviewed_detail("invalid TPSL trigger direction")),
    };
    offset += 1;
    let entire_position = match data.get(offset) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(unreviewed_detail("invalid TPSL close mode")),
    };
    offset += 1;
    let counter = u64_at(data, &mut offset)?;
    let request_time = i64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(bad_response)?
            .try_into()
            .map_err(|_| bad_response())?,
    );
    offset += 8;
    if offset != data.len() || counter == 0 || request_time <= 0 || trigger_price == 0 {
        return Err(unreviewed_detail("invalid pinned TPSL create parameters"));
    }
    Ok(CreateTpslParams {
        collateral_delta,
        size_delta,
        trigger_price,
        trigger_above,
        entire_position,
        counter,
        request_time,
    })
}

pub(super) fn operation_definitions() -> Vec<Definition> {
    let output = json!({
        "type":"object",
        "properties":{
            "state":{"enum":["request_submitted","keeper_pending","settled"]},
            "request":{"type":"string"},
            "position":{"type":"string"},
            "request_type":{"enum":["tp","sl"]},
            "trigger_price_usd_atoms":{"type":"string"},
            "signature":{"type":"string"},
            "settlement":{"type":"object"}
        },
        "required":["state","request","position","request_type","trigger_price_usd_atoms"],
        "additionalProperties":false
    });
    let error_schema = json!({"type":"object","required":["code","stage","message","next_action"]});
    vec![
        Definition {
            name: OP_TPSL_CREATE.into(),
            description: "Place one take-profit or stop-loss request on an existing Jupiter Perps position. The request remains pending until keeper execution or exact cancellation is finalized.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object","properties":{
                "network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},
                "position":{"type":"string","minLength":32,"maxLength":44},"market":{"enum":["SOL","ETH","BTC"]},"side":{"enum":["long","short"]},"request_type":{"enum":["tp","sl"]},
                "protection_parent_operation_id":{"type":"string","minLength":1,"description":"Exact unresolved GoFin operation ID of the opposite TP/SL request to pair on this position."},
                "trigger_price_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"receive_token":{"enum":["BTC","ETH","SOL","USDC"]},
                "entire_position":{"type":"boolean"},"size_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},
                "maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},
                "maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},
                "maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$"}
            },"required":["network","cluster","wallet","request_type","trigger_price_usd_atoms","receive_token","entire_position","maximum_protocol_fee_usd_atoms","maximum_network_fee_lamports","maximum_account_funding_lamports"],"allOf":[{"oneOf":[{"required":["position"]},{"required":["market","side"]}]}],"additionalProperties":false}),
            output_schema: output.clone(), error_schema: error_schema.clone(),
            examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","position":"11111111111111111111111111111111","request_type":"tp","trigger_price_usd_atoms":"120000000","receive_token":"USDC","entire_position":true,"maximum_protocol_fee_usd_atoms":"10000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})],
            coverage: "Pins one instantCreateTpsl instruction, exact position/request PDA and finalized keeper outcome; placement does not claim a completed trade.".into(),
            completion: "The request is Pending while open; terminal settlement requires a finalized keeper fill, cancellation, or expiry proof.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"instantCreateTpsl","request_type":"trigger","request_change":"decrease"}),
        },
        Definition {
            name: OP_TPSL_UPDATE.into(),
            description: "Change the trigger price or size of an open Jupiter Perps TP/SL request while preserving its original request identity.".into(),
            kind: OperationKind::Write, execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"request":{"anyOf":[{"type":"string","minLength":32,"maxLength":44},{"type":"null"}],"description":"Exact request PDA; null for a named-plan recovery placeholder."},"original_operation_id":{"anyOf":[{"type":"string","minLength":1},{"type":"null"}],"description":"Exact pending TP/SL create operation ID; null for named-plan recovery."},"original_submission":{"type":["object","null"],"description":"Optional exact saved create submission echo."},"trigger_price_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"}},"required":["network","cluster","wallet","request","original_operation_id","trigger_price_usd_atoms","maximum_network_fee_lamports"],"additionalProperties":false}),
            output_schema: output.clone(), error_schema: error_schema.clone(),
            examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","request":"11111111111111111111111111111111","original_operation_id":"operation-from-gofin-status","trigger_price_usd_atoms":"121000000","maximum_network_fee_lamports":"10000"})],
            coverage: "Pins the instantUpdateTpsl instruction to the exact open request and the separately approved trigger amendment.".into(),
            completion: "The amendment is complete only after its exact transaction finalizes; the original request remains pending.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"instantUpdateTpsl","request_identity":"preserved"}),
        },
        Definition {
            name: OP_TPSL_CANCEL.into(),
            description: "Cancel an open Jupiter Perps TP/SL request after proving its exact request account and owner.".into(),
            kind: OperationKind::Write, execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"request":{"anyOf":[{"type":"string","minLength":32,"maxLength":44},{"type":"null"}],"description":"Exact request PDA; null for named-plan recovery."},"original_operation_id":{"anyOf":[{"type":"string","minLength":1},{"type":"null"}],"description":"Exact pending TP/SL create operation ID; null for named-plan recovery."},"original_submission":{"type":["object","null"],"description":"Optional exact saved create submission echo."},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"}},"required":["network","cluster","wallet","request","original_operation_id","maximum_network_fee_lamports"],"additionalProperties":false}),
            output_schema: output, error_schema,
            examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","request":"11111111111111111111111111111111","original_operation_id":"operation-from-gofin-status","maximum_network_fee_lamports":"10000"})],
            coverage: "Pins a closePositionRequest instruction to the exact unexecuted trigger request and reconciles its finalized close event and account refunds.".into(),
            completion: "Cancellation is complete only after finalized request closure with executed=false and exact refund evidence.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"closePositionRequest3","request_type":"trigger"}),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entire_position_null_size_accepts_only_zero_or_exact_anchor_size() {
        assert!(tpsl_request_size_matches(true, 0, 12_100_000));
        assert!(tpsl_request_size_matches(true, 12_100_000, 12_100_000));
        assert!(!tpsl_request_size_matches(true, 12_099_999, 12_100_000));
        assert!(!tpsl_request_size_matches(false, 0, 12_100_000));
        assert!(tpsl_request_size_matches(false, 12_100_000, 12_100_000));
    }

    #[test]
    fn protection_candidate_filter_excludes_only_own_unsubmitted_prepare() {
        let input = json!({"request":"same","wallet":"w"});
        let record = |status: Status, submitted: bool, effects: bool| OperationRecord {
            id: "op".into(),
            request_id: "request".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_TPSL_CREATE.into(),
            input: input.clone(),
            status,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            preview: Some(
                serde_json::from_value(json!({
                    "operation":OP_TPSL_CREATE,"input":input,"binding":{"request":"same"},
                    "summary":"s","checks":[],"warnings":[],"observed_at":"2026-01-01T00:00:00Z",
                    "expires_at":"2026-01-01T00:01:00Z","valuation":null,
                    "intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]},
                    "operation_version":1
                }))
                .unwrap(),
            ),
            approval: None,
            submission: submitted.then(|| gofin_domain::Submission {
                kind: "solana".into(),
                reference: "sig".into(),
                data: json!({}),
            }),
            dispatch_evidence: None,
            position_baseline: None,
            effects: effects.then(|| gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: gofin_domain::Submission {
                    kind: "solana".into(),
                    reference: "sig".into(),
                    data: json!({}),
                },
                effects: vec![],
                evidence: json!({}),
                observed_at: "2026-01-01T00:00:00Z".into(),
                proof_digest: "digest".into(),
            }),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let awaiting = record(Status::AwaitingApproval, false, false);
        assert!(!is_unresolved_protection_candidate(&awaiting, &input, None));

        // A submitted identical input is durable authority and must remain a
        // candidate; input equality alone must never hide it.
        let submitted = record(Status::Pending, true, true);
        assert!(is_unresolved_protection_candidate(&submitted, &input, None));
        assert!(!is_unresolved_protection_candidate(
            &submitted,
            &input,
            Some("same")
        ));
    }

    #[test]
    fn update_receipt_uses_message_keys_for_signer_identity() {
        // Signer prefix order follows the message header (fee payer first),
        // not the Perps instruction's keeper/apiKeeper/owner role order.
        let keys = vec!["wallet".into(), "keeper".into(), "api-keeper".into()];
        let signatures = vec!["sig-wallet".into(), "sig-keeper".into(), "sig-api".into()];
        let expected = ["keeper", "api-keeper", "wallet"];
        assert!(update_receipt_signers_match(
            &keys,
            &signatures,
            "sig-wallet",
            "wallet",
            &expected,
        ));
        assert!(!update_receipt_signers_match(
            &keys,
            &signatures,
            "sig-wallet",
            "wallet",
            &["wallet", "keeper", "wrong-api"],
        ));
        assert!(!update_receipt_signers_match(
            &keys,
            &signatures,
            "different-transaction-signature",
            "wallet",
            &expected,
        ));
        assert!(!update_receipt_signers_match(
            &["wallet".into(), "keeper".into(), "keeper".into()],
            &signatures,
            "sig-wallet",
            "wallet",
            &expected,
        ));
    }

    #[test]
    fn child_permission_candidates_bind_parent_and_trigger_range() {
        let submission = json!({"kind":"solana","reference":"sig","data":{}});
        let spec = BoundarySpec {
            fixed: json!({
                "operation":OP_TPSL_UPDATE,"network":NETWORK,"cluster":CLUSTER,
                "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":"wallet",
                "wallet_address":"wallet-address","request":"request-address",
                "position":"position-address","parent_operation_id":"parent-op",
                "parent_submission":submission,"recovery_parent":false,
                "trigger_price_usd_atoms":Value::Null,
                "trigger_range":{"minimum_trigger_price_usd_atoms":"100","maximum_trigger_price_usd_atoms":"300"}
            }),
            parameters: json!({"maximum_network_fee_lamports":"50"}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: String::new(),
        };
        let prepared = PreparedOperation {
            operation: OP_TPSL_UPDATE.into(),
            input: json!({"wallet":"wallet","trigger_price_usd_atoms":"200"}),
            binding: json!({
                "wallet_address":"wallet-address","request":"request-address",
                "position":"position-address","parent_operation_id":"parent-op",
                "parent_submission":submission,"trigger_price_usd_atoms":"200",
                "maximum_network_fee_lamports":"25"
            }),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            action_deadline: None,
            operation_version: 1,
        };
        assert!(validate_child_permission_candidate(&spec, &prepared).is_ok());
        let mut changed = prepared.clone();
        changed.binding["trigger_price_usd_atoms"] = json!("301");
        assert!(validate_child_permission_candidate(&spec, &changed).is_err());
        changed = prepared;
        changed.binding["parent_operation_id"] = json!("other-parent");
        assert!(validate_child_permission_candidate(&spec, &changed).is_err());
    }

    #[test]
    fn recovery_parent_aliases_cannot_diverge_from_authoritative_binding() {
        let submission = json!({"kind":"solana","reference":"sig","data":{}});
        let binding = json!({
            "parent_operation_id":"parent-op",
            "parent_submission":submission,
            "original_operation_id":"parent-op",
            "original_submission":submission
        });
        assert!(validate_recovery_parent_aliases(&binding).is_ok());
        assert!(
            validate_recovery_parent_aliases(&json!({
                "parent_operation_id":"parent-op",
                "parent_submission":submission
            }))
            .is_ok(),
            "pre-alias durable records remain recoverable"
        );
        assert!(
            validate_recovery_parent_aliases(&json!({
                "parent_operation_id":"parent-op",
                "parent_submission":submission,
                "original_operation_id":"other-parent",
                "original_submission":submission
            }))
            .is_err()
        );
        assert!(
            validate_recovery_parent_aliases(&json!({
                "parent_operation_id":"parent-op",
                "parent_submission":submission,
                "original_operation_id":"parent-op",
                "original_submission":{"kind":"solana","reference":"other-sig","data":{}}
            }))
            .is_err()
        );
    }

    #[test]
    fn definitions_are_bounded_and_single_request_scoped() {
        let definitions = operation_definitions();
        assert_eq!(definitions.len(), 3);
        assert!(
            definitions
                .iter()
                .all(|definition| definition.input_schema["additionalProperties"] == false)
        );
        assert_eq!(
            definitions[0].input_schema["properties"]["request_type"]["enum"],
            json!(["tp", "sl"])
        );
        assert_eq!(
            definitions[0].input_schema["properties"]["maximum_protocol_fee_usd_atoms"]["pattern"],
            "^[0-9]+$"
        );
    }

    #[test]
    fn tpsl_reductions_reduce_unsigned_anchor_for_both_sides() {
        assert_eq!(
            tpsl_position_delta(&gofin_domain::PositionKind::DerivativeLong, 7).unwrap(),
            -7
        );
        assert_eq!(
            tpsl_position_delta(&gofin_domain::PositionKind::DerivativeShort, 7).unwrap(),
            -7
        );
        assert!(tpsl_position_delta(&gofin_domain::PositionKind::Investment, 7).is_err());
    }

    #[test]
    fn create_wire_params_match_pinned_idl_width_order_and_sentinels() {
        let mut data = instruction_discriminator("instantCreateTpsl").to_vec();
        data.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        data.extend_from_slice(&0x1112_1314_1516_1718u64.to_le_bytes());
        data.extend_from_slice(&0x2122_2324_2526_2728u64.to_le_bytes());
        data.push(1);
        data.push(0);
        data.extend_from_slice(&0x3132_3334_3536_3738u64.to_le_bytes());
        data.extend_from_slice(&0x4142_4344_4546_4748i64.to_le_bytes());
        assert_eq!(
            decode_create_params(&data).unwrap(),
            CreateTpslParams {
                collateral_delta: 0x0102_0304_0506_0708,
                size_delta: 0x1112_1314_1516_1718,
                trigger_price: 0x2122_2324_2526_2728,
                trigger_above: true,
                entire_position: false,
                counter: 0x3132_3334_3536_3738,
                request_time: 0x4142_4344_4546_4748,
            }
        );
        data.push(0);
        assert!(decode_create_params(&data).is_err());
    }

    #[test]
    fn create_account_roles_and_param_field_order_match_tracked_idl() {
        const IDL: &str = include_str!("../../resources/perps-published-idl.json");
        let idl: Value = serde_json::from_str(IDL).unwrap();
        let instruction = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|instruction| instruction["name"] == "instantCreateTpsl")
            .unwrap();
        let accounts = instruction["accounts"].as_array().unwrap();
        let names = accounts
            .iter()
            .map(|account| account["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "keeper",
                "apiKeeper",
                "owner",
                "receivingAccount",
                "perpetuals",
                "pool",
                "position",
                "positionRequest",
                "positionRequestAta",
                "custody",
                "custodyDovesPriceAccount",
                "custodyPythnetPriceAccount",
                "collateralCustody",
                "desiredMint",
                "referral",
                "tokenProgram",
                "associatedTokenProgram",
                "systemProgram",
                "eventAuthority",
                "program"
            ]
        );
        let privilege = accounts
            .iter()
            .map(|account| {
                (
                    account["isSigner"].as_bool().unwrap_or(false),
                    account["isMut"].as_bool().unwrap_or(false),
                )
            })
            .collect::<Vec<_>>();
        let mut expected = vec![(false, false); 20];
        expected[0] = (true, false);
        expected[1] = (true, false);
        expected[2] = (true, true);
        expected[3] = (false, true);
        expected[5] = (false, true);
        expected[7] = (false, true);
        expected[8] = (false, true);
        assert_eq!(privilege, expected);
        let params_name = instruction["args"][0]["type"]["defined"].as_str().unwrap();
        let params = idl["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ty| ty["name"] == params_name)
            .unwrap();
        let fields = params["type"]["fields"].as_array().unwrap();
        let field_names = fields
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            field_names,
            [
                "collateralUsdDelta",
                "sizeUsdDelta",
                "triggerPrice",
                "triggerAboveThreshold",
                "entirePosition",
                "counter",
                "requestTime"
            ]
        );
    }

    #[test]
    fn tpsl_update_oracle_indices_and_custody_offsets_follow_pinned_idl() {
        const IDL: &str = include_str!("../../resources/perps-published-idl.json");
        let idl: Value = serde_json::from_str(IDL).unwrap();
        let update = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|instruction| instruction["name"] == "instantUpdateTpsl")
            .unwrap();
        let account_names = update["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|account| account["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(account_names[8], "custodyDovesPriceAccount");
        assert_eq!(account_names[9], "custodyPythnetPriceAccount");

        fn fixed_size(ty: &Value, idl: &Value) -> Option<usize> {
            if let Some(name) = ty.as_str() {
                return Some(match name {
                    "u8" | "i8" | "bool" => 1,
                    "u16" | "i16" => 2,
                    "u32" | "i32" | "f32" => 4,
                    "u64" | "i64" | "f64" => 8,
                    "u128" | "i128" => 16,
                    "publicKey" => 32,
                    _ => return None,
                });
            }
            if let Some(array) = ty.get("array").and_then(Value::as_array) {
                let element = fixed_size(array.first()?, idl)?;
                return element.checked_mul(usize::try_from(array.get(1)?.as_u64()?).ok()?);
            }
            let name = ty.get("defined")?.as_str()?;
            let definition = idl["types"]
                .as_array()?
                .iter()
                .find(|item| item["name"] == name)?;
            let definition = &definition["type"];
            match definition["kind"].as_str()? {
                "struct" => definition["fields"]
                    .as_array()?
                    .iter()
                    .try_fold(0usize, |total, field| {
                        total.checked_add(fixed_size(&field["type"], idl)?)
                    }),
                "enum" => {
                    let largest = definition["variants"]
                        .as_array()?
                        .iter()
                        .map(|variant| {
                            variant["fields"].as_array().map_or(Some(0usize), |fields| {
                                fields.iter().try_fold(0usize, |total, field| {
                                    let field_type = field.get("type").unwrap_or(field);
                                    total.checked_add(fixed_size(field_type, idl)?)
                                })
                            })
                        })
                        .collect::<Option<Vec<_>>>()?
                        .into_iter()
                        .max()
                        .unwrap_or(0);
                    1usize.checked_add(largest)
                }
                _ => None,
            }
        }

        let custody = idl["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["name"] == "Custody")
            .unwrap();
        let fields = custody["type"]["fields"].as_array().unwrap();
        let mut offset = 8usize; // Anchor account discriminator.
        let mut oracle_offset = None;
        let mut doves_offset = None;
        for field in fields {
            match field["name"].as_str().unwrap() {
                "oracle" => oracle_offset = Some(offset),
                "dovesOracle" => doves_offset = Some(offset),
                _ => {}
            }
            offset = offset
                .checked_add(fixed_size(&field["type"], &idl).unwrap())
                .unwrap();
        }
        assert_eq!(oracle_offset, Some(106));
        assert_eq!(doves_offset, Some(320));
    }
}
