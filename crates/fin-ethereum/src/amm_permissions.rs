//! Reviewed variation scopes for single-pool exact-input AMM swaps.
//!
//! This module only defines authorization metadata and candidate checks. The
//! operation adapter remains responsible for quotes, simulation and dispatch.

use super::*;
use fin_amm::{
    AmmQuote, AmmSwapRequest, PermissionRequirement, QuoteEvidence, RateFloor, RouteIdentity,
    minimum_output,
};
use fin_domain::{
    AccountingLocation, CostBound, CostTreatment, ResourceAmount,
    boundaries::{BoundarySpec, BoundaryViolation, violation_error},
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_DEADLINE_SECONDS: u64 = 600;
const MAX_DEADLINE_SECONDS: u64 = 600;
const V3_FEE_TIERS: &[u32] = &[100, 500, 3_000, 10_000];

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SwapBounds {
    #[schemars(description = "Maximum input amount in human-readable units of the input token.")]
    pub max_input: String,
    #[schemars(description = "Maximum native transaction fee in human-readable ETH units.")]
    pub max_fee_eth: String,
    #[schemars(
        range(max = 1000),
        description = "Maximum selected swap slippage in basis points; at most 1000 bps (10%)."
    )]
    pub max_slippage_bps: u16,
    #[schemars(
        range(max = 10000),
        description = "Maximum permitted quoted price impact in basis points."
    )]
    pub max_price_impact_bps: u32,
    #[schemars(
        description = "Minimum output-atoms per input-atom rational rate. Both values are positive integers in atomic units."
    )]
    pub minimum_rate: RateFloor,
    #[serde(default)]
    #[schemars(
        description = "Optional absolute minimum output in human-readable output-token units."
    )]
    pub absolute_minimum_output: Option<String>,
    #[schemars(
        description = "Allowed Uniswap v3 fee tiers; must be empty for v4 and Ramses, whose pool identity is fixed by its key or tick spacing."
    )]
    pub allowed_fee_tiers: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Optional maximum pinned Ramses pool fee in ppm; this is a quote-time ceiling, not an on-chain fee cap."
    )]
    pub max_pool_fee_ppm: Option<u32>,
    #[serde(default = "default_deadline_seconds")]
    #[schemars(
        range(min = 1, max = 600),
        description = "Maximum deadline horizon in seconds, capped at 600."
    )]
    pub max_deadline_seconds: u64,
}
fn default_deadline_seconds() -> u64 {
    DEFAULT_DEADLINE_SECONDS
}

pub(crate) fn swap_bounds_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(SwapBounds))
        .expect("swap bounds schema is serializable")
}

fn parse_bounds(
    raw: &Value,
    input_decimals: u8,
    output_decimals: u8,
    protocol: &str,
) -> Result<SwapBounds> {
    let mut bounds: SwapBounds = serde_json::from_value(raw.clone()).map_err(|_| {
        FinError::validation("Invalid AMM swap bounds; inspect the boundary schema.")
    })?;
    if bounds.max_slippage_bps > 1_000
        || bounds.max_price_impact_bps > 10_000
        || bounds.max_deadline_seconds == 0
        || bounds.max_deadline_seconds > MAX_DEADLINE_SECONDS
        || bounds.minimum_rate.input_atoms.is_zero()
        || bounds.minimum_rate.output_atoms.is_zero()
    {
        return Err(FinError::validation(
            "AMM slippage, price impact, rate floor or deadline exceeds its valid range.",
        ));
    }
    let max_input = Quantity::from_decimal(&bounds.max_input, input_decimals)?;
    let max_fee = Quantity::from_decimal(&bounds.max_fee_eth, 18)?;
    if max_input.is_zero() || max_fee.is_zero() {
        return Err(FinError::validation(
            "AMM input and native fee bounds must be positive.",
        ));
    }
    bounds.max_input = max_input.to_decimal(input_decimals);
    bounds.max_fee_eth = max_fee.to_decimal(18);
    if let Some(value) = bounds.absolute_minimum_output.as_mut() {
        let absolute = Quantity::from_decimal(value, output_decimals)?;
        if absolute.is_zero() {
            return Err(FinError::validation(
                "An explicit absolute output floor must be positive.",
            ));
        }
        *value = absolute.to_decimal(output_decimals);
    }
    let mut tiers = bounds.allowed_fee_tiers.clone();
    tiers.sort_unstable();
    tiers.dedup();
    if tiers.len() != bounds.allowed_fee_tiers.len() {
        return Err(FinError::validation(
            "Allowed AMM fee tiers must not contain duplicates.",
        ));
    }
    match protocol {
        "v3" if tiers.is_empty() || tiers.iter().any(|tier| !V3_FEE_TIERS.contains(tier)) => {
            return Err(FinError::validation(
                "Uniswap v3 requires one or more supported fee tiers.",
            ));
        }
        "v4" if !tiers.is_empty() => {
            return Err(FinError::validation(
                "Uniswap v4 pool keys fix the fee; allowed_fee_tiers must be empty.",
            ));
        }
        "ramses" if !tiers.is_empty() => {
            return Err(FinError::validation(
                "Ramses pool identity uses tick spacing, so allowed_fee_tiers must be empty.",
            ));
        }
        "ramses" => {}
        "v3" | "v4" => {}
        _ => {
            return Err(FinError::validation(
                "Unsupported AMM protocol in swap permission.",
            ));
        }
    }
    bounds.allowed_fee_tiers = tiers;
    if bounds.max_pool_fee_ppm.is_some_and(|fee| fee >= 1_000_000) {
        return Err(FinError::validation(
            "Ramses maximum quoted pool fee must be below one million ppm.",
        ));
    }
    Ok(bounds)
}

fn fail(field: &str, approved: Value, candidate: Value) -> FinError {
    violation_error(
        &format!("AMM boundary {field} was exceeded."),
        BoundaryViolation {
            constraint: field.into(),
            approved_bound: approved,
            candidate_value: candidate,
            evidence_time: None,
            reauthorization_required: true,
            reconciliation_required: false,
        },
    )
}
fn canonical_token(reference: &str) -> Result<String> {
    if reference == "native" {
        Ok(reference.to_owned())
    } else {
        Ok(lower_address(address(reference)?))
    }
}

fn atoms(decimal: &str, decimals: u8) -> Result<Quantity> {
    Quantity::from_decimal(decimal, decimals)
}
fn typed_amount(asset: &AssetId, decimal: &str) -> Result<Amount> {
    Ok(Amount {
        asset: asset.clone(),
        atoms: atoms(decimal, asset.decimals)?,
    })
}
async fn token_asset_at(adapter: &EthereumAdapter, reference: &str, block: u64) -> Result<AssetId> {
    if reference == "native" {
        return Ok(adapter.native_asset());
    }
    let token = address(reference)?;
    require(
        token != Address::ZERO,
        "ZERO_ADDRESS",
        "AMM token identity must be nonzero.",
    )?;
    adapter.runtime.code_at(token, Some(block)).await?;
    let decimals = adapter
        .runtime
        .call(token, &abi::Erc20::decimalsCall {}, Some(block))
        .await?;
    pow10(decimals)?;
    Ok(adapter.asset(&lower_address(token), decimals))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayLabels {
    input: String,
    output: String,
}

fn safe_display_label(symbol: Option<&str>) -> String {
    const MAX_LABEL_BYTES: usize = 32;
    match symbol {
        Some(value)
            if !value.is_empty()
                && value.as_bytes()[0].is_ascii_alphanumeric()
                && value.len() <= MAX_LABEL_BYTES
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                }) =>
        {
            value.to_owned()
        }
        _ => "token".to_owned(),
    }
}

async fn display_label_at(adapter: &EthereumAdapter, reference: &str, block: u64) -> String {
    // The financial identity and decimals have already been validated at this
    // block. Symbol metadata is presentation-only: unavailable or unsafe
    // metadata must never invalidate an otherwise valid operation.
    let symbol = adapter
        .token(reference, Some(block))
        .await
        .ok()
        .map(|token| token.symbol);
    safe_display_label(symbol.as_deref())
}

fn display_labels_from_prepared(summary: &str) -> DisplayLabels {
    let fallback = || DisplayLabels {
        input: "token".into(),
        output: "token".into(),
    };
    let Some(body) = summary.strip_prefix("Swap ") else {
        return fallback();
    };
    let Some((input_part, output_part)) = body.split_once(" for at least ") else {
        return fallback();
    };
    let Some((_, input_symbol)) = input_part.split_once(' ') else {
        return fallback();
    };
    let Some((_, output_label)) = output_part.split_once(' ') else {
        return fallback();
    };
    let Some((output_symbol, _)) = output_label.split_once(';') else {
        return fallback();
    };
    if output_symbol.is_empty() {
        return fallback();
    }
    DisplayLabels {
        input: safe_display_label(Some(input_symbol)),
        output: safe_display_label(Some(output_symbol)),
    }
}

async fn request_identity(
    adapter: &EthereumAdapter,
    input: &Value,
    operation: &str,
) -> Result<(request::Request, Value, AssetId, AssetId, DisplayLabels)> {
    let config = adapter.config.uniswap.as_ref();
    let is_ramses = operation == "ramses.swap";
    let request = request::parse(operation, input)?;
    let wallet = adapter.resolve(&request, true)?;
    let owner = address(&wallet.address)?;
    let resource = resource(adapter.config.chain_id, owner);
    let protocol = if is_ramses {
        "ramses"
    } else {
        request.protocol.as_deref().unwrap_or("v3")
    };
    let v3_profile = if is_ramses {
        super::uniswap::profile::V3DeploymentProfile::ramses(adapter.ramses()?)
    } else {
        super::uniswap::profile::V3DeploymentProfile::uniswap(adapter.uniswap()?)
    };
    let router = match protocol {
        "v3" | "ramses" => address(v3_profile.router)?,
        "v4" => address(
            &config
                .ok_or_else(|| FinError::validation("Uniswap is not configured."))?
                .v4
                .as_ref()
                .ok_or_else(|| {
                    FinError::new(
                        "OPERATION_UNAVAILABLE",
                        "authorization",
                        "Uniswap v4 is not configured.",
                        "Select a configured protocol.",
                    )
                })?
                .router,
        )?,
        _ => return Err(FinError::validation("Unsupported Uniswap protocol.")),
    };
    let recipient = address(
        request
            .recipient
            .as_deref()
            .ok_or_else(|| FinError::validation("Swap recipient is required."))?,
    )?;
    if recipient != owner || recipient == Address::ZERO {
        return Err(FinError::validation(
            "Swap authorization requires output to the signing wallet.",
        ));
    }
    let block = fin_evm::rpc_u64(
        &adapter
            .runtime
            .rpc
            .request("eth_blockNumber", json!([]))
            .await?,
    )?;
    let token_in =
        token_asset_at(adapter, request.token_in.as_deref().expect("parsed"), block).await?;
    let token_out = token_asset_at(
        adapter,
        request.token_out.as_deref().expect("parsed"),
        block,
    )
    .await?;
    let labels = DisplayLabels {
        input: display_label_at(adapter, request.token_in.as_deref().expect("parsed"), block).await,
        output: display_label_at(
            adapter,
            request.token_out.as_deref().expect("parsed"),
            block,
        )
        .await,
    };
    let input_router = if protocol == "v3" || protocol == "ramses" {
        address(v3_profile.router)?
    } else {
        address(
            &config
                .expect("configured above")
                .v4
                .as_ref()
                .expect("configured above")
                .router,
        )?
    };
    adapter.runtime.code_at(input_router, Some(block)).await?;
    if protocol == "v3" || protocol == "ramses" {
        adapter
            .runtime
            .code_at(address(v3_profile.factory)?, Some(block))
            .await?;
        adapter
            .runtime
            .code_at(address(v3_profile.quoter)?, Some(block))
            .await?;
        if protocol == "ramses" {
            let config = adapter.ramses()?;
            let expected_deployer = address(&config.pool_deployer)?;
            let observed = adapter
                .runtime
                .call(
                    address(&config.factory)?,
                    &abi::RamsesFactory::ramsesV3PoolDeployerCall {},
                    Some(block),
                )
                .await?;
            if observed != expected_deployer {
                return Err(FinError::validation(
                    "Ramses factory deployer differs from reviewed config.",
                ));
            }
            let router_deployer = adapter
                .runtime
                .call(
                    address(&config.router)?,
                    &abi::RamsesRouter::deployerCall {},
                    Some(block),
                )
                .await?;
            let quoter_deployer = adapter
                .runtime
                .call(
                    address(&config.quoter)?,
                    &abi::RamsesQuoter::deployerCall {},
                    Some(block),
                )
                .await?;
            if router_deployer != expected_deployer || quoter_deployer != expected_deployer {
                return Err(FinError::validation(
                    "Ramses router and quoter do not share the reviewed pool-deployer identity.",
                ));
            }
        }
        if request.token_in.as_deref() == Some("native")
            || request.token_out.as_deref() == Some("native")
        {
            adapter.v3_weth(Some(block)).await?;
        }
    } else {
        let v4 = config
            .and_then(|config| config.v4.as_ref())
            .expect("configured above");
        adapter
            .runtime
            .code_at(address(&v4.quoter)?, Some(block))
            .await?;
        adapter
            .runtime
            .code_at(address(&v4.pool_manager)?, Some(block))
            .await?;
    }
    let v4_key = request.v4_pool_key.clone();
    if protocol == "v4" {
        super::uniswap::v4::pool_key(v4_key.as_ref().expect("parser enforces v4 pool key"))?;
    }
    let mut fixed = json!({
        "operation":operation, "network":format!("ethereum:{}", adapter.config.chain_id),
        "chain_id":adapter.config.chain_id, "wallet":request.wallet, "resource":resource,
        "signer":lower_address(owner), "recipient":lower_address(recipient),
        "router":lower_address(router), "protocol":if is_ramses {"v3"} else {protocol},
        "token_in":token_in, "token_out":token_out,
        "v4_pool_key":v4_key,
    });
    if is_ramses {
        let amount_text = if request.amount.as_deref() == Some("all") {
            request.max_amount.as_deref().expect("validated all cap")
        } else {
            request.amount.as_deref().expect("validated amount")
        };
        let quote = adapter
            .quote_protocol(crate::reads::ProtocolQuoteRequest {
                dex: "ramses",
                input: request.token_in.as_deref().expect("parsed"),
                output: request.token_out.as_deref().expect("parsed"),
                amount: amount_text,
                fee_tier: None,
                tick_spacing: request.tick_spacing,
                protocol: None,
                v4_key: None,
                block: Some(block),
            })
            .await?;
        fixed["dex"] = json!("ramses");
        fixed["tick_spacing"] = json!(request.tick_spacing);
        fixed["pool"] = json!(quote.pool);
        fixed["factory"] = json!(adapter.ramses()?.factory);
        fixed["pool_deployer"] = json!(adapter.ramses()?.pool_deployer);
        fixed["initial_pool_fee_ppm"] = json!(quote.swap_fee_ppm);
    }
    Ok((request, fixed, token_in, token_out, labels))
}

fn fixed_from_prepared(prepared: &PreparedOperation) -> Result<Value> {
    if !matches!(prepared.operation.as_str(), "uniswap.swap" | "ramses.swap") {
        return Err(FinError::validation(
            "AMM boundary applies only to configured single-pool swaps.",
        ));
    }
    let request = request::parse(&prepared.operation, &prepared.input)?;
    let b: Binding = serde_json::from_value(prepared.binding.clone())
        .map_err(|_| FinError::validation("Prepared AMM binding is invalid."))?;
    let is_ramses = b.dex.as_deref() == Some("ramses") || prepared.operation == "ramses.swap";
    let protocol = b.protocol.as_deref().unwrap_or("v3");
    let router = match protocol {
        "v3" => b.transaction.to.clone(),
        "v4" => b.transaction.to.clone(),
        _ => return Err(FinError::validation("Prepared AMM protocol is invalid.")),
    };
    let signer = b.transaction.sender.clone();
    let owner = address(&signer)?;
    if b.recipient.as_deref() != Some(signer.as_str()) {
        return Err(FinError::validation(
            "Prepared swap does not return output to its signing wallet.",
        ));
    }
    let output = b
        .output
        .clone()
        .ok_or_else(|| FinError::validation("Prepared swap output identity is missing."))?;
    let expected_resource = resource(b.transaction.chain_id, owner);
    if b.resource != expected_resource
        || b.token.network != format!("ethereum:{}", b.transaction.chain_id)
        || output.network != b.token.network
    {
        return Err(FinError::validation(
            "Prepared swap custody or asset network identity is inconsistent.",
        ));
    }
    if b.transaction.chain_id != request.chain_id
        || request.protocol.as_deref().unwrap_or("v3") != protocol
        || request
            .recipient
            .as_deref()
            .map(|value| address(value).map(lower_address))
            .transpose()?
            .as_deref()
            != b.recipient.as_deref()
        || canonical_token(
            request
                .token_in
                .as_deref()
                .expect("parser validates token_in"),
        )? != b.token.reference
        || canonical_token(
            request
                .token_out
                .as_deref()
                .expect("parser validates token_out"),
        )? != output.reference
        || request.v4_pool_key != b.v4_pool_key
        || (is_ramses && request.tick_spacing != b.tick_spacing)
    {
        return Err(FinError::validation(
            "Prepared swap identity differs from its complete request.",
        ));
    }
    let mut fixed = json!({
        "operation":prepared.operation, "network":b.token.network, "chain_id":b.transaction.chain_id,
        "wallet":request.wallet, "resource":b.resource, "signer":signer,
        "recipient":b.recipient, "router":router, "protocol":protocol,
        "token_in":b.token, "token_out":output, "v4_pool_key":b.v4_pool_key,
    });
    if is_ramses {
        fixed["dex"] = json!("ramses");
        fixed["tick_spacing"] = json!(b.tick_spacing);
        fixed["pool"] = json!(b.pool);
        fixed["factory"] = json!(b.factory);
        fixed["pool_deployer"] = json!(b.pool_deployer);
    }
    Ok(fixed)
}

fn make_spec(
    fixed: Value,
    bounds: SwapBounds,
    input_asset: AssetId,
    output_asset: AssetId,
    resource: ResourceId,
    labels: &DisplayLabels,
) -> Result<BoundarySpec> {
    let max_input = typed_amount(&input_asset, &bounds.max_input)?;
    let fee = Quantity::from_decimal(&bounds.max_fee_eth, 18)?;
    let native = AssetId {
        network: input_asset.network.clone(),
        reference: "native".into(),
        decimals: 18,
    };
    let fee_amount = Amount {
        asset: native,
        atoms: fee.clone(),
    };
    let parameters = serde_json::to_value(&bounds)
        .map_err(|_| FinError::validation("AMM boundary parameters could not be encoded."))?;
    let protocol = fixed["protocol"].as_str().unwrap_or("");
    let dex = fixed["dex"].as_str().unwrap_or("uniswap");
    let route = if protocol == "v4" {
        let key: V4PoolKey = serde_json::from_value(fixed["v4_pool_key"].clone())
            .map_err(|_| FinError::validation("Fixed v4 pool key is invalid."))?;
        format!(
            "Uniswap v4 pool-key fee {} ppm, tick spacing {}",
            key.fee, key.tick_spacing
        )
    } else if dex == "ramses" {
        format!(
            "Ramses pool spacing {} (maximum observed fee {} ppm)",
            fixed["tick_spacing"],
            bounds.max_pool_fee_ppm.unwrap_or_default()
        )
    } else {
        format!(
            "Uniswap v3 allowed fee tiers {:?} ppm",
            bounds.allowed_fee_tiers
        )
    };
    let permission = if input_asset.reference == "native" {
        "native input needs no token allowance".to_owned()
    } else if protocol == "v4" {
        format!(
            "finite {} allowance to canonical Permit2 and finite Permit2 allowance or exact Permit2 permit to router {}",
            input_asset.reference, fixed["router"]
        )
    } else {
        format!(
            "finite {} allowance or exact EIP-2612 permit to router {}",
            input_asset.reference, fixed["router"]
        )
    };
    let rate_output = bounds
        .minimum_rate
        .output_atoms
        .to_decimal(output_asset.decimals);
    let rate_input = bounds
        .minimum_rate
        .input_atoms
        .to_decimal(input_asset.decimals);
    let absolute_floor = bounds
        .absolute_minimum_output
        .as_deref()
        .map(|value| {
            format!(
                "; absolute minimum output {value} {}",
                output_asset.reference
            )
        })
        .unwrap_or_default();
    let summary = format!(
        "Swap up to {} {} for {}; recipient {}; router {}; {route}; fixed minimum rate {rate_output} {} per {rate_input} {}; max native fee {} ETH; max selected slippage {} bps; max price impact {} bps; deadline within {} seconds; permission scope: {permission}{absolute_floor}.",
        bounds.max_input,
        labels.input,
        labels.output,
        fixed["recipient"],
        fixed["router"],
        output_asset.reference,
        input_asset.reference,
        bounds.max_fee_eth,
        bounds.max_slippage_bps,
        bounds.max_price_impact_bps,
        bounds.max_deadline_seconds,
    );
    Ok(BoundarySpec {
        fixed,
        parameters,
        maximum_debits: vec![ResourceAmount {
            resource: resource.clone(),
            amount: max_input,
        }],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash { resource },
            amount: fee_amount,
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary,
    })
}

impl EthereumAdapter {
    pub(crate) async fn swap_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        raw_bounds: &Value,
    ) -> Result<PlanPermission> {
        if !matches!(operation, "uniswap.swap" | "ramses.swap") {
            return Err(FinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "AMM plan permission supports reviewed Uniswap and Ramses swaps only.",
                "Choose a supported swap step.",
            ));
        }
        self.runtime.assert_chain().await?;
        let (request, fixed, input_asset, output_asset, labels) =
            request_identity(self, input, operation).await?;
        let protocol = fixed["protocol"].as_str().unwrap_or("v3").to_owned();
        let bounds_protocol = fixed["dex"].as_str().unwrap_or(&protocol).to_owned();
        let mut bounds = parse_bounds(
            raw_bounds,
            input_asset.decimals,
            output_asset.decimals,
            &bounds_protocol,
        )?;
        let mut fixed = fixed;
        if bounds_protocol == "ramses" {
            let observed_fee = fixed["initial_pool_fee_ppm"]
                .as_u64()
                .and_then(|fee| u32::try_from(fee).ok())
                .ok_or_else(|| FinError::validation("Pinned Ramses pool fee is missing."))?;
            let ceiling = bounds
                .max_pool_fee_ppm
                .or(request.max_pool_fee_ppm)
                .unwrap_or(observed_fee);
            if request
                .max_pool_fee_ppm
                .is_some_and(|requested| requested > ceiling)
            {
                return Err(FinError::validation(
                    "Requested Ramses quote fee ceiling exceeds the approved plan bound.",
                ));
            }
            if observed_fee > ceiling {
                return Err(FinError::validation(
                    "Pinned Ramses pool fee exceeds the approved quote-time ceiling.",
                ));
            }
            bounds.max_pool_fee_ppm = Some(ceiling);
            fixed
                .as_object_mut()
                .expect("fixed object")
                .remove("initial_pool_fee_ppm");
        }
        let cap = Quantity::from_decimal(&bounds.max_input, input_asset.decimals)?;
        let initial_text = if request.amount.as_deref() == Some("all") {
            request
                .max_amount
                .as_deref()
                .expect("parser requires max_amount")
        } else {
            request.amount.as_deref().expect("required amount")
        };
        let initial = atoms(initial_text, input_asset.decimals)?;
        let initial_fee = Quantity::from_decimal(
            request.max_fee_eth.as_deref().expect("required max fee"),
            18,
        )?;
        if initial > cap || initial_fee > Quantity::from_decimal(&bounds.max_fee_eth, 18)? {
            return Err(FinError::validation(
                "Initial swap exceeds its requested plan caps.",
            ));
        }
        let initial_minimum = Quantity::from_decimal(
            request.min_output.as_deref().expect("required min_output"),
            output_asset.decimals,
        )?;
        let fixed_rate_floor = initial.mul_div_ceil(
            &bounds.minimum_rate.output_atoms,
            &bounds.minimum_rate.input_atoms,
        )?;
        let absolute_floor = bounds
            .absolute_minimum_output
            .as_deref()
            .map(|value| Quantity::from_decimal(value, output_asset.decimals))
            .transpose()?
            .unwrap_or_default();
        if initial_minimum.is_zero() || initial_minimum < fixed_rate_floor.max(absolute_floor) {
            return Err(FinError::validation(
                "Initial swap minimum output is below its plan rate or absolute floor.",
            ));
        }
        let current = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default();
        let deadline = request.deadline.expect("required deadline");
        if deadline <= current || deadline > current.saturating_add(bounds.max_deadline_seconds) {
            return Err(FinError::validation(
                "Initial swap deadline exceeds the approved relative deadline bound.",
            ));
        }
        if request.slippage_bps.unwrap_or(100) > bounds.max_slippage_bps {
            return Err(FinError::validation(
                "Initial swap slippage exceeds its plan cap.",
            ));
        }
        if request.max_price_impact_bps.unwrap_or(10_000) > bounds.max_price_impact_bps {
            return Err(FinError::validation(
                "Initial swap price impact threshold exceeds its plan cap.",
            ));
        }
        if protocol == "v3"
            && request
                .fee_tier
                .is_some_and(|tier| !bounds.allowed_fee_tiers.contains(&tier))
        {
            return Err(FinError::validation(
                "Initial v3 fee tier is outside allowed_fee_tiers.",
            ));
        }
        if protocol == "v4" && request.fee_tier.is_some() {
            return Err(FinError::validation(
                "Uniswap v4 fee is fixed by its explicit pool key.",
            ));
        }
        let resource: ResourceId = serde_json::from_value(fixed["resource"].clone())
            .map_err(|_| FinError::validation("Physical resource identity is invalid."))?;
        let spec = make_spec(
            fixed,
            bounds,
            input_asset.clone(),
            output_asset.clone(),
            resource.clone(),
            &labels,
        )?;
        Ok(PlanPermission {
            spec,
            possible_credits: vec![FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: output_asset,
            }],
            fee_assets: vec![AssetId {
                network: input_asset.network,
                reference: "native".into(),
                decimals: 18,
            }],
            // AMM attempts bind short-lived quotes and deadlines, so one
            // parent plan authorization must not outlive the reviewed
            // six-hundred-second operation horizon. Per-step fixed identity,
            // debit and cost bounds remain in `spec`; the plan engine applies
            // its aggregate quantity limits across the plan's steps.
            authorization_seconds: Some(MAX_DEADLINE_SECONDS),
            cumulative_scope: None,
        })
    }

    pub(crate) fn swap_boundaries(
        &self,
        prepared: &PreparedOperation,
        raw_bounds: &Value,
    ) -> Result<BoundarySpec> {
        let fixed = fixed_from_prepared(prepared)?;
        let input_asset: AssetId = serde_json::from_value(fixed["token_in"].clone())
            .map_err(|_| FinError::validation("Input asset identity is invalid."))?;
        let output_asset: AssetId = serde_json::from_value(fixed["token_out"].clone())
            .map_err(|_| FinError::validation("Output asset identity is invalid."))?;
        let bounds_protocol = fixed["dex"]
            .as_str()
            .or_else(|| fixed["protocol"].as_str())
            .unwrap_or("v3")
            .to_owned();
        let resource: ResourceId = serde_json::from_value(fixed["resource"].clone())
            .map_err(|_| FinError::validation("Physical resource identity is invalid."))?;
        let mut bounds = parse_bounds(
            raw_bounds,
            input_asset.decimals,
            output_asset.decimals,
            &bounds_protocol,
        )?;
        if bounds_protocol == "ramses" && bounds.max_pool_fee_ppm.is_none() {
            let binding: Binding = serde_json::from_value(prepared.binding.clone())
                .map_err(|_| FinError::validation("Prepared Ramses binding is invalid."))?;
            bounds.max_pool_fee_ppm = binding.max_pool_fee_ppm.or(binding.swap_fee_ppm);
        }
        let labels = display_labels_from_prepared(&prepared.summary);
        let spec = make_spec(fixed, bounds, input_asset, output_asset, resource, &labels)?;
        self.validate_swap_candidate(&spec, prepared)?;
        Ok(spec)
    }

    pub(crate) fn validate_swap_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        validate_candidate(spec, prepared)
    }
}

fn validate_candidate(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<()> {
    let fixed = fixed_from_prepared(prepared)?;
    if fixed != spec.fixed {
        return Err(fail("fixed identity", spec.fixed.clone(), fixed));
    }
    let input_asset: AssetId = serde_json::from_value(spec.fixed["token_in"].clone())
        .map_err(|_| FinError::validation("Boundary input asset is invalid."))?;
    let output_asset: AssetId = serde_json::from_value(spec.fixed["token_out"].clone())
        .map_err(|_| FinError::validation("Boundary output asset is invalid."))?;
    let bounds = parse_bounds(
        &spec.parameters,
        input_asset.decimals,
        output_asset.decimals,
        spec.fixed["dex"]
            .as_str()
            .unwrap_or(spec.fixed["protocol"].as_str().unwrap_or("v3")),
    )?;
    let b: Binding = serde_json::from_value(prepared.binding.clone())
        .map_err(|_| FinError::validation("Prepared AMM binding is invalid."))?;
    let request = request::parse(&prepared.operation, &prepared.input)?;
    let units = b.units.clone();
    let max_input = Quantity::from_decimal(&bounds.max_input, input_asset.decimals)?;
    if units > max_input {
        return Err(fail(
            "max_input",
            json!(bounds.max_input),
            json!(units.to_decimal(input_asset.decimals)),
        ));
    }
    let requested_input = if request.amount.as_deref() == Some("all") {
        request
            .max_amount
            .as_deref()
            .expect("parser validates max_amount")
    } else {
        request.amount.as_deref().expect("parser validates amount")
    };
    let requested_cap = Quantity::from_decimal(requested_input, input_asset.decimals)?;
    if requested_cap > max_input
        || (request.amount.as_deref() != Some("all") && requested_cap != units)
    {
        return Err(fail(
            "max_input",
            json!(bounds.max_input),
            json!(requested_input),
        ));
    }
    if request.deadline != b.deadline {
        return Err(fail("deadline", json!(b.deadline), json!(request.deadline)));
    }
    if request.slippage_bps.unwrap_or(100) > bounds.max_slippage_bps {
        return Err(fail(
            "max_slippage_bps",
            json!(bounds.max_slippage_bps),
            json!(request.slippage_bps),
        ));
    }
    if spec.fixed["dex"].as_str() == Some("ramses") {
        let spacing = b.tick_spacing.ok_or_else(|| {
            FinError::validation("Prepared Ramses pool is missing its fixed tick spacing.")
        })?;
        if request.tick_spacing != Some(spacing) {
            return Err(fail(
                "tick_spacing",
                json!(spacing),
                json!(request.tick_spacing),
            ));
        }
        let observed_fee = b
            .swap_fee_ppm
            .ok_or_else(|| FinError::validation("Prepared Ramses pool fee quote is missing."))?;
        let fee_cap = bounds.max_pool_fee_ppm.unwrap_or_default();
        let prepared_fee_cap = b.max_pool_fee_ppm.or(b.swap_fee_ppm).unwrap_or_default();
        let candidate_fee_cap = request.max_pool_fee_ppm.unwrap_or(prepared_fee_cap);
        if candidate_fee_cap > fee_cap || candidate_fee_cap > prepared_fee_cap {
            return Err(fail(
                "max_pool_fee_ppm",
                json!({"approved":fee_cap,"prepared":prepared_fee_cap}),
                json!(candidate_fee_cap),
            ));
        }
        if observed_fee > fee_cap {
            return Err(fail(
                "max_pool_fee_ppm",
                json!(fee_cap),
                json!(observed_fee),
            ));
        }
    } else if request.protocol.as_deref().unwrap_or("v3") == "v3" {
        let tier = b.fee_tier.ok_or_else(|| {
            FinError::validation("Prepared v3 swap is missing its exact fee tier.")
        })?;
        if !bounds.allowed_fee_tiers.contains(&tier) {
            return Err(fail(
                "allowed_fee_tiers",
                json!(bounds.allowed_fee_tiers),
                json!(tier),
            ));
        }
    } else if request.fee_tier.is_some() {
        return Err(fail("v4_fixed_fee", json!(null), json!(request.fee_tier)));
    }
    let impact = b.price_impact_bps.ok_or_else(|| {
        FinError::validation("Prepared AMM quote is missing integer price impact evidence.")
    })?;
    if impact > bounds.max_price_impact_bps {
        return Err(fail(
            "max_price_impact_bps",
            json!(bounds.max_price_impact_bps),
            json!(impact),
        ));
    }
    if request.max_price_impact_bps.unwrap_or(10_000) > bounds.max_price_impact_bps {
        return Err(fail(
            "max_price_impact_bps",
            json!(bounds.max_price_impact_bps),
            json!(request.max_price_impact_bps),
        ));
    }
    let quote_output = b
        .quote_output
        .clone()
        .ok_or_else(|| FinError::validation("Prepared AMM quote is missing exact output units."))?;
    let rate_floor = bounds.minimum_rate.clone();
    let absolute = bounds
        .absolute_minimum_output
        .as_deref()
        .map(|value| Quantity::from_decimal(value, output_asset.decimals))
        .transpose()?;
    let route = RouteIdentity {
        dex: "uniswap".into(),
        version: request.protocol.clone().unwrap_or_else(|| "v3".into()),
        assets: vec![input_asset.clone(), output_asset.clone()],
        fingerprint: b.pool.clone().unwrap_or_default(),
    };
    let fee_cap = Amount {
        asset: AssetId {
            network: input_asset.network.clone(),
            reference: "native".into(),
            decimals: 18,
        },
        atoms: Quantity::from_decimal(&bounds.max_fee_eth, 18)?,
    };
    let request_fee = Quantity::from_decimal(
        request
            .max_fee_eth
            .as_deref()
            .expect("write requires max_fee_eth"),
        18,
    )?;
    if request_fee > fee_cap.atoms {
        return Err(fail(
            "max_fee_eth",
            json!(bounds.max_fee_eth),
            json!(request_fee.to_decimal(18)),
        ));
    }
    let expected_value = if input_asset.reference == "native" {
        units.clone()
    } else {
        Quantity::zero()
    };
    if b.transaction.value != expected_value || b.transaction.calldata == "0x" {
        return Err(fail(
            "transaction_envelope",
            json!({"value":expected_value,"nonempty_calldata":true}),
            json!({"value":b.transaction.value,"calldata":b.transaction.calldata}),
        ));
    }
    let transaction_fee = fin_evm::quantity(b.transaction.maximum_cost()?);
    let amount = Amount {
        asset: input_asset.clone(),
        atoms: units.clone(),
    };
    let out = Amount {
        asset: output_asset.clone(),
        atoms: quote_output.clone(),
    };
    let fee_actual = prepared
        .intent
        .maximum_costs
        .iter()
        .find(|fee| fee.amount.asset.reference == "native" && fee.amount.asset.decimals == 18)
        .map(|fee| fee.amount.clone())
        .ok_or_else(|| FinError::validation("Prepared swap is missing its native fee cap."))?;
    if fee_actual.asset != fee_cap.asset
        || fee_actual.atoms > fee_cap.atoms
        || fee_actual.atoms != transaction_fee
    {
        return Err(fail(
            "max_fee_eth",
            json!(bounds.max_fee_eth),
            json!(fee_actual.atoms.to_decimal(18)),
        ));
    }
    let fee_route = RouteIdentity {
        assets: route.assets.clone(),
        ..route
    };
    let swap = AmmSwapRequest {
        resource: b.resource.clone(),
        approved_resource: b.resource.clone(),
        quote: AmmQuote {
            resource: b.resource.clone(),
            route: fee_route.clone(),
            input: amount,
            output: out,
            maximum_fee: fee_actual,
            price_impact_bps: impact,
            permission: PermissionRequirement::None,
            evidence: QuoteEvidence {
                observed_at: prepared.observed_at.clone(),
                block_number: None,
                block_hash: None,
            },
        },
        approved_route: fee_route,
        selected_slippage_bps: request.slippage_bps.unwrap_or(100),
        approved_slippage_bps: bounds.max_slippage_bps,
        rate_floor: Some(rate_floor),
        absolute_minimum_output: absolute,
        input_cap: Amount {
            asset: input_asset,
            atoms: max_input,
        },
        fee_cap,
        maximum_price_impact_bps: bounds.max_price_impact_bps,
    };
    let minimum = minimum_output(&swap)?;
    let actual_min = b
        .minimum_output
        .clone()
        .ok_or_else(|| FinError::validation("Prepared swap is missing minimum output."))?;
    if actual_min < minimum || actual_min > quote_output {
        return Err(fail(
            "minimum_output",
            json!(minimum.to_decimal(output_asset.decimals)),
            json!(actual_min.to_decimal(output_asset.decimals)),
        ));
    }
    let deadline = b
        .deadline
        .ok_or_else(|| FinError::validation("Prepared swap deadline is missing."))?;
    if request.deadline != Some(deadline) {
        return Err(fail("deadline", json!(deadline), json!(request.deadline)));
    }
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default();
    if deadline <= now || deadline > now.saturating_add(bounds.max_deadline_seconds) {
        return Err(fail(
            "max_deadline_seconds",
            json!(bounds.max_deadline_seconds),
            json!(deadline.saturating_sub(now)),
        ));
    }
    let router = spec.fixed["router"]
        .as_str()
        .ok_or_else(|| FinError::validation("Approved router identity is missing."))?;
    if b.transaction.to != router {
        return Err(fail("router", json!(router), json!(b.transaction.to)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(deadline: u64) -> Value {
        json!({
            "wallet":"main", "network":"evm", "chain_id":1, "request_id":"swap-1", "max_fee_eth":"0.001",
            "token_in":"native", "token_out":"0x2222222222222222222222222222222222222222", "amount":"1",
            "recipient":"0x1111111111111111111111111111111111111111", "min_output":"0.000099",
            "slippage_bps":100, "max_price_impact_bps":40, "deadline":deadline, "protocol":"v3"
        })
    }

    #[test]
    fn display_labels_are_bounded_safe_and_fall_back_without_changing_identity() {
        assert_eq!(safe_display_label(Some("USDG-2")), "USDG-2");
        assert_eq!(safe_display_label(Some("ETH")), "ETH");
        assert_eq!(safe_display_label(Some("-")), "token");
        assert_eq!(safe_display_label(None), "token");
        assert_eq!(safe_display_label(Some("USDC; send funds")), "token");
        assert_eq!(safe_display_label(Some("US\u{202e}DC")), "token");
        let too_long = "A".repeat(33);
        assert_eq!(safe_display_label(Some(&too_long)), "token");
        assert_eq!(
            display_labels_from_prepared(
                "Swap 1 ETH for at least 2 USDC; recipient 0x1111; router 0x2222."
            ),
            DisplayLabels {
                input: "ETH".into(),
                output: "USDC".into(),
            }
        );
        assert_eq!(
            display_labels_from_prepared("untrusted or changed summary"),
            DisplayLabels {
                input: "token".into(),
                output: "token".into(),
            }
        );
        assert_eq!(
            display_labels_from_prepared(
                "Swap 1 USDC; ignore review for at least 2 USDG; recipient wallet."
            ),
            DisplayLabels {
                input: "token".into(),
                output: "USDG".into(),
            }
        );
    }

    fn prepared(deadline: u64) -> PreparedOperation {
        let resource = ResourceId("ethereum:1:0x1111111111111111111111111111111111111111".into());
        let native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let token_out = AssetId {
            network: "ethereum:1".into(),
            reference: "0x2222222222222222222222222222222222222222".into(),
            decimals: 6,
        };
        let tx = FrozenTransaction {
            chain_id: 1,
            sender: "0x1111111111111111111111111111111111111111".into(),
            to: "0x4444444444444444444444444444444444444444".into(),
            value: Quantity::from_decimal("1", 18).unwrap(),
            calldata: "0x1234".into(),
            nonce: 1,
            gas: 100,
            max_fee_per_gas: Quantity::from_u64(1),
            max_priority_fee_per_gas: Quantity::from_u64(1),
        };
        let binding = Binding {
            transaction: tx.clone(),
            resource: resource.clone(),
            token: native.clone(),
            units: Quantity::from_decimal("1", 18).unwrap(),
            recipient: Some(tx.sender.clone()),
            spender: Some(tx.to.clone()),
            output: Some(token_out.clone()),
            minimum_output: Some(Quantity::new("99").unwrap()),
            deadline: Some(deadline),
            pool: Some("pool-3000".into()),
            fee_tier: Some(3000),
            dex: None,
            tick_spacing: None,
            pool_deployer: None,
            factory: None,
            swap_fee_ppm: None,
            max_pool_fee_ppm: None,
            protocol: Some("v3".into()),
            v4_pool_key: None,
            route_token_in: Some("weth".into()),
            route_token_out: Some(token_out.reference.clone()),
            quote_output: Some(Quantity::new("100").unwrap()),
            price_impact_bps: Some(20),
            permit_template: None,
            permit2_allowance_before: None,
            allowance_before: None,
            supplied_before: None,
            debt_before: None,
            collateral_before: None,
            enabled: None,
            a_token: None,
            debt_token: None,
            scaled_before: None,
            repay_all: false,
            all_amount: false,
            supply_anchor: None,
            debt_anchor: None,
            debt_rounding_maximum: None,
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        };
        PreparedOperation {
            operation: "uniswap.swap".into(),
            input: input(deadline),
            binding: serde_json::to_value(binding).unwrap(),
            summary: "test".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: chrono::Utc::now().to_rfc3339(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: native.clone(),
                        atoms: Quantity::from_decimal("1", 18).unwrap(),
                    },
                }],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: native,
                        atoms: Quantity::from_u64(100),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected: vec![],
                locks: vec![resource],
            },
            operation_version: 1,
        }
    }
    fn bounds() -> Value {
        json!({"max_input":"1.5","max_fee_eth":"0.001","max_slippage_bps":100,"max_price_impact_bps":40,
            "minimum_rate":{"output_atoms":"90","input_atoms":"1000000000000000000"},"absolute_minimum_output":"0.000091",
            "allowed_fee_tiers":[3000],"max_deadline_seconds":600})
    }

    #[test]
    fn bounds_are_strict_and_protocol_fee_tier_rules_are_enforced() {
        assert!(parse_bounds(&bounds(), 18, 6, "v3").is_ok());
        let mut excessive_slippage = bounds();
        excessive_slippage["max_slippage_bps"] = json!(1001);
        assert!(parse_bounds(&excessive_slippage, 18, 6, "v3").is_err());
        let mut invalid = bounds();
        invalid["surprise"] = json!(true);
        assert!(parse_bounds(&invalid, 18, 6, "v3").is_err());
        assert!(parse_bounds(&json!({"max_input":"1.5","max_fee_eth":"0.001","max_slippage_bps":100,"max_price_impact_bps":40,"minimum_rate":{"output_atoms":"90","input_atoms":"1"},"absolute_minimum_output":null,"allowed_fee_tiers":[3000],"max_deadline_seconds":601}),18,6,"v3").is_err());
        let mut v4 = bounds();
        v4["allowed_fee_tiers"] = json!([]);
        assert!(parse_bounds(&v4, 18, 6, "v4").is_ok());
        assert!(parse_bounds(&bounds(), 18, 6, "v4").is_err());
    }

    #[test]
    fn swap_boundary_schema_documents_units_and_selected_slippage_cap() {
        let schema = swap_bounds_schema();
        assert!(
            schema["properties"]["max_input"]["description"]
                .as_str()
                .unwrap()
                .contains("human-readable")
        );
        assert!(
            schema["properties"]["minimum_rate"]["description"]
                .as_str()
                .unwrap()
                .contains("atomic units")
        );
        assert_eq!(
            schema["properties"]["max_slippage_bps"]["maximum"],
            json!(1000)
        );
    }

    #[test]
    fn prepared_candidate_checks_shared_exact_floor_fee_impact_and_deadline() {
        let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap() + 300;
        let candidate = prepared(now);
        let fixed = fixed_from_prepared(&candidate).unwrap();
        let input_asset: AssetId = serde_json::from_value(fixed["token_in"].clone()).unwrap();
        let output_asset: AssetId = serde_json::from_value(fixed["token_out"].clone()).unwrap();
        let resource: ResourceId = serde_json::from_value(fixed["resource"].clone()).unwrap();
        let parsed = parse_bounds(&bounds(), 18, 6, "v3").unwrap();
        let spec = make_spec(
            fixed,
            parsed,
            input_asset,
            output_asset,
            resource,
            &DisplayLabels {
                input: "ETH".into(),
                output: "USDC".into(),
            },
        )
        .unwrap();
        assert!(spec.summary.starts_with("Swap up to 1.5 ETH for USDC;"));
        assert!(spec.fixed.get("display_labels").is_none());
        assert!(validate_candidate(&spec, &candidate).is_ok());
        let mut bad = candidate.clone();
        bad.binding["price_impact_bps"] = json!(41);
        assert_eq!(
            validate_candidate(&spec, &bad).unwrap_err().code,
            "BOUNDARY_VIOLATION"
        );
        let mut bad = candidate.clone();
        bad.binding["minimum_output"] = json!("90");
        assert_eq!(
            validate_candidate(&spec, &bad).unwrap_err().code,
            "BOUNDARY_VIOLATION"
        );
        let mut bad = candidate.clone();
        bad.binding["deadline"] = json!(now + 601);
        bad.input["deadline"] = json!(now + 601);
        assert_eq!(
            validate_candidate(&spec, &bad).unwrap_err().code,
            "BOUNDARY_VIOLATION"
        );
    }
}
