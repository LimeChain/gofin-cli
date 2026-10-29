//! Provider-bound financial authority for hosted Uniswap routes.
//!
//! Limits use trusted quote metadata; opaque execution payloads are not decoded.
use super::super::*;
use super::{ENDPOINT, PROVIDER, parse_binding};
use crate::amm_permissions::{
    DisplayLabels, canonical_token, fail, parse_bounds, token_identity_at,
};
use gofin_domain::boundaries::BoundarySpec;

pub(crate) fn bounds_schema() -> Value {
    let mut schema = crate::amm_permissions::swap_bounds_schema();
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.remove("allowed_fee_tiers");
        properties.remove("max_pool_fee_ppm");
    }
    if let Some(required) = schema["required"].as_array_mut() {
        required.retain(|field| field != "allowed_fee_tiers" && field != "max_pool_fee_ppm");
    }
    schema
}

pub(crate) async fn identity(
    adapter: &EthereumAdapter,
    input: &Value,
) -> Result<(
    request::Request,
    Value,
    AssetId,
    AssetId,
    DisplayLabels,
    u64,
)> {
    let request = request::parse_plan_template("uniswap.swap", input)?;
    let wallet = adapter.resolve(&request, true)?;
    let owner = address(&wallet.address)?;
    let recipient = address(request.recipient.as_deref().expect("parsed recipient"))?;
    require(
        owner != Address::ZERO && owner == recipient,
        "SWAP_RECIPIENT_UNSUPPORTED",
        "Swap authorization requires output to the signing wallet.",
    )?;
    adapter.runtime.assert_chain().await?;
    let block = gofin_evm::rpc_u64(
        &adapter
            .runtime
            .rpc
            .request("eth_blockNumber", json!([]))
            .await?,
    )?;
    let ((input_asset, input_label), (output_asset, output_label)) = tokio::try_join!(
        token_identity_at(
            adapter,
            request.token_in.as_deref().expect("parsed token"),
            block
        ),
        token_identity_at(
            adapter,
            request.token_out.as_deref().expect("parsed token"),
            block
        ),
    )?;
    require(
        input_asset.reference != output_asset.reference,
        "SWAP_ASSET_INVALID",
        "Swap assets must be distinct.",
    )?;
    let fixed = json!({
        "provider":PROVIDER,"endpoint":ENDPOINT,"operation":"uniswap.swap","protocol":"api",
        "network":format!("ethereum:{}",adapter.config.chain_id),"chain_id":adapter.config.chain_id,
        "wallet":request.wallet,"resource":resource(adapter.config.chain_id,owner),
        "signer":lower_address(owner),"recipient":lower_address(recipient),
        "token_in":input_asset,"token_out":output_asset,
        "route_preference":request.route_preference.as_deref().unwrap_or("auto"),
    });
    Ok((
        request,
        fixed,
        input_asset,
        output_asset,
        DisplayLabels {
            input: input_label,
            output: output_label,
        },
        block,
    ))
}

pub(crate) fn fixed_from_prepared(prepared: &PreparedOperation) -> Result<Value> {
    let b = parse_binding(prepared)?;
    let input = if prepared.operation == "swap.execute" {
        request::concrete_swap_execute_input("uniswap", &prepared.input)?
    } else {
        prepared.input.clone()
    };
    let request = request::parse("uniswap.swap", &input)?;
    let owner = address(&b.owner)?;
    let recipient = address(request.recipient.as_deref().expect("parsed recipient"))?;
    require(
        b.provider == PROVIDER
            && b.endpoint == ENDPOINT
            && b.chain_id == request.chain_id
            && b.resource == resource(request.chain_id, owner)
            && owner == recipient
            && owner != Address::ZERO
            && b.token.network == format!("ethereum:{}", request.chain_id)
            && b.output.network == b.token.network
            && b.token.reference
                == canonical_token(request.token_in.as_deref().expect("parsed token"))?
            && b.output.reference
                == canonical_token(request.token_out.as_deref().expect("parsed token"))?
            && b.token.reference != b.output.reference
            && b.route_preference == request.route_preference.as_deref().unwrap_or("auto"),
        "UNISWAP_API_IDENTITY_MISMATCH",
        "Prepared Uniswap API identity differs from its request.",
    )?;
    Ok(json!({
        "provider":PROVIDER,"endpoint":ENDPOINT,"operation":"uniswap.swap","protocol":"api",
        "network":b.token.network,"chain_id":request.chain_id,"wallet":request.wallet,
        "resource":b.resource,"signer":lower_address(owner),"recipient":lower_address(recipient),
        "token_in":b.token,"token_out":b.output,"route_preference":b.route_preference,
    }))
}

pub(crate) fn validate_candidate_at(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let fixed = fixed_from_prepared(prepared)?;
    // In particular, historical v3/v4 scopes cannot acquire hosted provider authority.
    if fixed != spec.fixed {
        return Err(fail("fixed identity", spec.fixed.clone(), fixed));
    }
    let b = parse_binding(prepared)?;
    let request = request::parse("uniswap.swap", &prepared.input)?;
    let bounds = parse_bounds(&spec.parameters, b.token.decimals, b.output.decimals, "api")?;
    let input_cap = Quantity::from_decimal(&bounds.max_input, b.token.decimals)?;
    let requested_input = if request.amount.as_deref() == Some("all") {
        request.max_amount.as_deref().expect("parsed maximum")
    } else {
        request.amount.as_deref().expect("parsed amount")
    };
    let requested_units = Quantity::from_decimal(requested_input, b.token.decimals)?;
    if b.units.is_zero()
        || b.units > input_cap
        || requested_units > input_cap
        || b.units > requested_units
        || (request.amount.as_deref() != Some("all") && b.units != requested_units)
    {
        return Err(fail(
            "max_input",
            json!(bounds.max_input),
            json!(requested_input),
        ));
    }
    if request.slippage_bps.unwrap_or(100) > bounds.max_slippage_bps {
        return Err(fail(
            "max_slippage_bps",
            json!(bounds.max_slippage_bps),
            json!(request.slippage_bps),
        ));
    }
    if b.price_impact_bps > bounds.max_price_impact_bps
        || request.max_price_impact_bps.unwrap_or(10_000) > bounds.max_price_impact_bps
    {
        return Err(fail(
            "max_price_impact_bps",
            json!(bounds.max_price_impact_bps),
            json!(b.price_impact_bps),
        ));
    }
    let fee_cap = Quantity::from_decimal(&bounds.max_fee_eth, 18)?;
    let requested_fee =
        Quantity::from_decimal(request.max_fee_eth.as_deref().expect("parsed fee"), 18)?;
    if requested_fee > fee_cap || b.maximum_fee > requested_fee {
        return Err(fail(
            "max_fee_eth",
            json!(bounds.max_fee_eth),
            json!(b.maximum_fee.to_decimal(18)),
        ));
    }
    let rate_floor = b.units.mul_div_ceil(
        &bounds.minimum_rate.output_atoms,
        &bounds.minimum_rate.input_atoms,
    )?;
    let absolute_floor = bounds
        .absolute_minimum_output
        .as_deref()
        .map(|value| Quantity::from_decimal(value, b.output.decimals))
        .transpose()?
        .unwrap_or_default();
    let requested_floor = Quantity::from_decimal(
        request.min_output.as_deref().expect("parsed floor"),
        b.output.decimals,
    )?;
    let minimum = rate_floor.max(absolute_floor).max(requested_floor);
    if b.minimum_output.is_zero() || b.minimum_output < minimum || b.minimum_output > b.quote_output
    {
        return Err(fail(
            "minimum_output",
            json!(minimum),
            json!(b.minimum_output),
        ));
    }
    let now = u64::try_from(at.timestamp()).unwrap_or_default();
    if request
        .deadline
        .is_none_or(|deadline| b.deadline > deadline)
        || b.deadline <= now
        || b.deadline > now.saturating_add(bounds.max_deadline_seconds)
    {
        return Err(fail(
            "max_deadline_seconds",
            json!(bounds.max_deadline_seconds),
            json!(b.deadline.saturating_sub(now)),
        ));
    }
    let observed = chrono::DateTime::parse_from_rfc3339(&prepared.observed_at).map_err(|_| {
        GoFinError::validation("Prepared Uniswap quote observation time is invalid.")
    })?;
    if observed > at || at.signed_duration_since(observed).num_seconds() > 120 {
        return Err(fail(
            "quote_freshness_seconds",
            json!(120),
            json!(at.signed_duration_since(observed).num_seconds()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_bounds_do_not_require_or_accept_local_pool_authority() {
        let bounds = json!({
            "max_input":"1", "max_fee_eth":"0.01", "max_slippage_bps":100,
            "max_price_impact_bps":1000,
            "minimum_rate":{"input_atoms":"1","output_atoms":"1"}
        });
        assert!(parse_bounds(&bounds, 18, 18, "api").is_ok());
        let mut local = bounds.clone();
        local["allowed_fee_tiers"] = json!([500]);
        assert!(parse_bounds(&local, 18, 18, "api").is_err());
        let mut local = bounds;
        local["max_pool_fee_ppm"] = json!(500);
        assert!(parse_bounds(&local, 18, 18, "api").is_err());
        assert!(
            bounds_schema()["properties"]
                .get("allowed_fee_tiers")
                .is_none()
        );
    }
}
