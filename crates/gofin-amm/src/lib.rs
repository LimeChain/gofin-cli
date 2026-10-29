//! Small protocol-independent contracts and safety checks for AMM swaps.
//! Implementations own transport, simulation, signing, and receipt evidence.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, EffectConstraint, GoFinError,
    PreparedOperation, Quantity, ResourceId, Result,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const MAX_QUOTE_AGE_SECONDS: i64 = 120;
const BPS: u64 = 10_000;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteIdentity {
    /// Stable integration identifier, such as a reviewed deployment name.
    pub dex: String,
    pub version: String,
    /// Ordered assets, including endpoints. Pool identifiers remain opaque.
    pub assets: Vec<AssetId>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuoteEvidence {
    pub observed_at: String,
    pub block_number: Option<u64>,
    pub block_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequirement {
    None,
    TokenApproval,
    Permit,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmmQuoteRequest {
    pub resource: ResourceId,
    pub input: Amount,
    pub output_asset: AssetId,
    pub route: Option<RouteIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmmQuote {
    pub resource: ResourceId,
    pub route: RouteIdentity,
    pub input: Amount,
    pub output: Amount,
    pub maximum_fee: Amount,
    /// Exact integer basis points; 10_000 means 100%.
    pub price_impact_bps: u32,
    pub permission: PermissionRequirement,
    pub evidence: QuoteEvidence,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RateFloor {
    pub output_atoms: Quantity,
    pub input_atoms: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmmSwapRequest {
    pub resource: ResourceId,
    pub approved_resource: ResourceId,
    pub quote: AmmQuote,
    pub approved_route: RouteIdentity,
    pub selected_slippage_bps: u16,
    pub approved_slippage_bps: u16,
    pub rate_floor: Option<RateFloor>,
    pub absolute_minimum_output: Option<Quantity>,
    pub input_cap: Amount,
    pub fee_cap: Amount,
    pub maximum_price_impact_bps: u32,
}

#[async_trait]
pub trait AmmDex: Send + Sync {
    async fn quote(&self, request: AmmQuoteRequest) -> Result<AmmQuote>;
    async fn prepare_swap(&self, request: AmmSwapRequest) -> Result<PreparedOperation>;
}

/// Render an AMM quote with exact human-unit quantities and its provenance.
/// Asset references are shown verbatim because shared code cannot assume symbols.
pub fn format_quote(quote: &AmmQuote) -> Result<String> {
    let observed = DateTime::parse_from_rfc3339(&quote.evidence.observed_at)
        .map_err(|_| stale("Quote observation time is invalid."))?;
    let valid_until = observed + chrono::Duration::seconds(MAX_QUOTE_AGE_SECONDS);
    let permission = match &quote.permission {
        PermissionRequirement::None => "none",
        PermissionRequirement::TokenApproval => "token approval",
        PermissionRequirement::Permit => "permit",
    };
    let version = quote.route.version.trim_start_matches('v');
    let fee = if quote.maximum_fee.atoms.is_zero() {
        "fees bounded during preparation".to_owned()
    } else {
        format!(
            "maximum fee {} {}",
            quote
                .maximum_fee
                .atoms
                .to_decimal(quote.maximum_fee.asset.decimals),
            quote.maximum_fee.asset.reference
        )
    };
    Ok(format!(
        "Quote on {} via {} v{}: {} {} → {} {}; {}; price impact {} bps; permission {}; observed {}, valid through {}",
        quote.input.asset.network,
        quote.route.dex,
        version,
        quote.input.atoms.to_decimal(quote.input.asset.decimals),
        quote.input.asset.reference,
        quote.output.atoms.to_decimal(quote.output.asset.decimals),
        quote.output.asset.reference,
        fee,
        quote.price_impact_bps,
        permission,
        observed.to_rfc3339(),
        valid_until.to_rfc3339(),
    ))
}

/// Calculate the strictest requested output floor, rounding each proportional
/// floor upward so tiny trades cannot round the safety bound down to zero.
pub fn minimum_output(request: &AmmSwapRequest) -> Result<Quantity> {
    if request.selected_slippage_bps > 10_000 {
        return Err(GoFinError::validation(
            "Selected slippage cannot exceed 10,000 basis points.",
        ));
    }
    let q = &request.quote;
    let slip_factor = Quantity::from_u64(BPS - u64::from(request.selected_slippage_bps));
    let bps = Quantity::from_u64(BPS);
    let mut floor = q.output.atoms.mul_div_ceil(&slip_factor, &bps)?;
    if let Some(rate) = &request.rate_floor {
        if rate.input_atoms.is_zero() || rate.output_atoms.is_zero() {
            return Err(GoFinError::validation(
                "Fixed-rate floor numerator and denominator must be positive.",
            ));
        }
        let rate_floor = q
            .input
            .atoms
            .mul_div_ceil(&rate.output_atoms, &rate.input_atoms)?;
        floor = floor.max(rate_floor);
    }
    if let Some(absolute) = &request.absolute_minimum_output {
        floor = floor.max(absolute.clone());
    }
    Ok(floor)
}

/// Validate a quote at preparation and again immediately before a swap.
pub fn validate_swap(request: &AmmSwapRequest, now: DateTime<Utc>) -> Result<Quantity> {
    let q = &request.quote;
    if request.selected_slippage_bps > request.approved_slippage_bps
        || request.approved_slippage_bps > 10_000
    {
        return Err(bound("Selected slippage exceeds the approved scope."));
    }
    if request.resource != request.approved_resource || q.resource != request.resource {
        return Err(bound(
            "Quote resource differs from the approved resource scope.",
        ));
    }
    if q.route != request.approved_route {
        return Err(bound(
            "Quote route differs from the approved route identity.",
        ));
    }
    validate_route(&q.route)?;
    validate_identifier(&request.resource.0, "Resource identity")?;
    if q.input.asset == q.output.asset || q.input.atoms.is_zero() || q.output.atoms.is_zero() {
        return Err(GoFinError::validation(
            "AMM input and output must be positive quantities of distinct assets.",
        ));
    }
    validate_asset(&q.input.asset)?;
    validate_asset(&q.output.asset)?;
    validate_asset(&q.maximum_fee.asset)?;
    if q.input.asset.network != q.output.asset.network
        || q.input.asset.network != q.maximum_fee.asset.network
    {
        return Err(bound(
            "Quote assets and fee must belong to the same network.",
        ));
    }
    if q.maximum_fee.atoms.is_zero() {
        return Err(GoFinError::validation("AMM maximum fee must be positive."));
    }
    if q.route.assets.first() != Some(&q.input.asset)
        || q.route.assets.last() != Some(&q.output.asset)
        || q.route.assets.len() < 2
    {
        return Err(bound(
            "Quote assets do not match the immutable route identity.",
        ));
    }
    if request.input_cap.asset != q.input.asset || q.input.atoms > request.input_cap.atoms {
        return Err(bound("Quote input exceeds the approved input cap."));
    }
    if request.fee_cap.asset != q.maximum_fee.asset || q.maximum_fee.atoms > request.fee_cap.atoms {
        return Err(bound("Quote fee exceeds the approved fee cap."));
    }
    if q.price_impact_bps > 10_000 || request.maximum_price_impact_bps > 10_000 {
        return Err(GoFinError::validation(
            "Price impact basis points cannot exceed 10,000.",
        ));
    }
    if q.price_impact_bps > request.maximum_price_impact_bps {
        return Err(
            bound("Quote price impact exceeds the approved maximum.").with_details(
                serde_json::json!({
                    "reason": "price_impact_exceeded",
                    "observed_price_impact_bps": q.price_impact_bps,
                    "maximum_price_impact_bps": request.maximum_price_impact_bps,
                }),
            ),
        );
    }
    if q.evidence.block_number.is_some() != q.evidence.block_hash.is_some() {
        return Err(GoFinError::validation(
            "Block number and block hash evidence must be supplied together.",
        ));
    }
    if let Some(hash) = &q.evidence.block_hash {
        validate_identifier(hash, "Block hash")?;
    }
    let observed = DateTime::parse_from_rfc3339(&q.evidence.observed_at)
        .map_err(|_| stale("Quote observation time is invalid."))?
        .with_timezone(&Utc);
    let age = now.signed_duration_since(observed);
    if age < chrono::Duration::zero() || age > chrono::Duration::seconds(MAX_QUOTE_AGE_SECONDS) {
        return Err(stale("Quote is from the future or older than 120 seconds."));
    }
    let minimum = minimum_output(request)?;
    if minimum > q.output.atoms {
        return Err(
            bound("Minimum output exceeds the quoted output.").with_details(serde_json::json!({
                "reason": "minimum_output_unattainable",
                "minimum_output_atoms": minimum,
                "quote_output_atoms": q.output.atoms,
                "output_asset": q.output.asset,
            })),
        );
    }
    Ok(minimum)
}

/// Check a refreshed output against the prepared floor; the approved floor is
/// immutable across requotes even when the current quote is better.
pub fn validate_refreshed_output(
    prepared_minimum: &Quantity,
    refreshed_output: &Quantity,
) -> Result<()> {
    if refreshed_output < prepared_minimum {
        return Err(
            bound("Refreshed output falls below the approved minimum-output floor.").with_details(
                serde_json::json!({
                    "reason": "minimum_output_unattainable",
                    "minimum_output_atoms": prepared_minimum,
                    "quote_output_atoms": refreshed_output,
                }),
            ),
        );
    }
    Ok(())
}

/// Revalidate a requote against the original approval. A requote may improve
/// output, but cannot replace the route, resource, input, fee asset, or impact
/// scope that was approved.
pub fn validate_refreshed_quote(
    prepared: &AmmSwapRequest,
    refreshed: &AmmQuote,
    now: DateTime<Utc>,
) -> Result<Quantity> {
    if refreshed.resource != prepared.approved_resource
        || refreshed.route != prepared.approved_route
        || refreshed.input != prepared.quote.input
        || refreshed.maximum_fee.asset != prepared.fee_cap.asset
    {
        return Err(bound(
            "Refreshed quote changed an approved identity or input.",
        ));
    }
    let prepared_minimum = minimum_output(prepared)?;
    let mut current = prepared.clone();
    current.quote = refreshed.clone();
    validate_swap(&current, now)?;
    validate_refreshed_output(&prepared_minimum, &refreshed.output.atoms)?;
    Ok(prepared_minimum)
}

/// Build the shared operation envelope after adapter-specific simulation and
/// identity checks. The binding records the exact floor and approval scope.
pub fn prepared_operation(
    request: &AmmSwapRequest,
    now: DateTime<Utc>,
) -> Result<PreparedOperation> {
    let minimum = validate_swap(request, now)?;
    let observed = DateTime::parse_from_rfc3339(&request.quote.evidence.observed_at)
        .map_err(|_| stale("Quote observation time is invalid."))?;
    let expires = observed + chrono::Duration::seconds(MAX_QUOTE_AGE_SECONDS);
    let intent = gofin_domain::FinancialIntent {
        debits: vec![gofin_domain::ResourceAmount {
            resource: request.resource.clone(),
            amount: request.quote.input.clone(),
        }],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash {
                resource: request.resource.clone(),
            },
            amount: request.quote.maximum_fee.clone(),
            treatment: CostTreatment::AdditionalDebit,
        }],
        expected: vec![EffectConstraint::Conversion {
            resource: request.resource.clone(),
            input: request.quote.input.asset.clone(),
            maximum_input: request.quote.input.atoms.clone(),
            output: request.quote.output.asset.clone(),
            minimum_output: minimum.clone(),
        }],
        locks: vec![request.resource.clone()],
    };
    Ok(PreparedOperation {
        operation: "amm.swap".into(),
        input: serde_json::to_value(request).map_err(|e| GoFinError::validation(e.to_string()))?,
        binding: json!({
            "route": request.quote.route,
            "input": request.quote.input,
            "output_asset": request.quote.output.asset,
            "minimum_output": minimum,
            "maximum_fee": request.quote.maximum_fee,
            "selected_slippage_bps": request.selected_slippage_bps,
            "approved_slippage_bps": request.approved_slippage_bps,
            "permission": request.quote.permission,
            "quote_evidence": request.quote.evidence,
        }),
        summary: format!(
            "{}; minimum output floor {} {}",
            format_quote(&request.quote)?,
            minimum.to_decimal(request.quote.output.asset.decimals),
            request.quote.output.asset.reference,
        ),
        checks: vec![],
        warnings: vec![],
        observed_at: observed.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        valuation: None,
        intent,
        operation_version: 1,
        action_deadline: None,
    })
}

fn validate_route(route: &RouteIdentity) -> Result<()> {
    validate_identifier(&route.dex, "DEX identity")?;
    validate_identifier(&route.version, "Route version")?;
    validate_identifier(&route.fingerprint, "Route fingerprint")?;
    if route.assets.len() < 2 {
        return Err(bound("Route must contain at least two assets."));
    }
    for asset in &route.assets {
        validate_asset(asset)?;
    }
    if route
        .assets
        .iter()
        .any(|asset| asset.network != route.assets[0].network)
    {
        return Err(bound("All assets in a route must belong to one network."));
    }
    Ok(())
}

fn validate_asset(asset: &AssetId) -> Result<()> {
    validate_identifier(&asset.network, "Asset network")?;
    validate_identifier(&asset.reference, "Asset reference")
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(GoFinError::validation(format!(
            "{label} must be a nonempty identifier of at most 256 bytes without control characters."
        )));
    }
    Ok(())
}

fn bound(message: &str) -> GoFinError {
    GoFinError::new(
        "AMM_BOUND_EXCEEDED",
        "preparation",
        message,
        "Request a new quote and approval with explicit bounds.",
    )
}
fn stale(message: &str) -> GoFinError {
    GoFinError::new(
        "AMM_QUOTE_STALE",
        "preparation",
        message,
        "Request a fresh AMM quote.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(reference: &str) -> AssetId {
        AssetId {
            network: "toy".into(),
            reference: reference.into(),
            decimals: 18,
        }
    }
    fn request() -> AmmSwapRequest {
        let input_asset = asset("coin-a");
        let output_asset = asset("coin-b");
        let route = RouteIdentity {
            dex: "toy-pool".into(),
            version: "1".into(),
            assets: vec![input_asset.clone(), output_asset.clone()],
            fingerprint: "route-abc".into(),
        };
        AmmSwapRequest {
            resource: ResourceId("wallet-1".into()),
            approved_resource: ResourceId("wallet-1".into()),
            quote: AmmQuote {
                resource: ResourceId("wallet-1".into()),
                route: route.clone(),
                input: Amount {
                    asset: input_asset,
                    atoms: Quantity::new("101").unwrap(),
                },
                output: Amount {
                    asset: output_asset,
                    atoms: Quantity::new("99").unwrap(),
                },
                maximum_fee: Amount {
                    asset: asset("gas"),
                    atoms: Quantity::new("2").unwrap(),
                },
                price_impact_bps: 20,
                permission: PermissionRequirement::TokenApproval,
                evidence: QuoteEvidence {
                    observed_at: "2026-01-01T00:00:00Z".into(),
                    block_number: Some(5),
                    block_hash: Some("toy-block".into()),
                },
            },
            approved_route: route,
            selected_slippage_bps: 100,
            approved_slippage_bps: 200,
            rate_floor: None,
            absolute_minimum_output: None,
            input_cap: Amount {
                asset: asset("coin-a"),
                atoms: Quantity::new("110").unwrap(),
            },
            fee_cap: Amount {
                asset: asset("gas"),
                atoms: Quantity::new("3").unwrap(),
            },
            maximum_price_impact_bps: 50,
        }
    }
    #[test]
    fn floors_round_up_at_one_atom_and_take_strictest_bound() {
        let mut r = request();
        r.quote.output.atoms = Quantity::new("1").unwrap();
        assert_eq!(minimum_output(&r).unwrap(), Quantity::new("1").unwrap());
        r.rate_floor = Some(RateFloor {
            output_atoms: Quantity::new("2").unwrap(),
            input_atoms: Quantity::new("3").unwrap(),
        });
        assert_eq!(minimum_output(&r).unwrap(), Quantity::new("68").unwrap());
        r.absolute_minimum_output = Some(Quantity::new("70").unwrap());
        assert_eq!(minimum_output(&r).unwrap(), Quantity::new("70").unwrap());
    }
    #[test]
    fn bounds_reject_drift_stale_future_and_cost_mismatch() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut r = request();
        let floor = validate_swap(&r, now).unwrap();
        let error = validate_refreshed_output(&floor, &Quantity::new("97").unwrap()).unwrap_err();
        assert_eq!(error.code, "AMM_BOUND_EXCEEDED");
        let details = error.details.unwrap();
        assert_eq!(details["minimum_output_atoms"], "99");
        assert_eq!(details["quote_output_atoms"], "97");
        r.quote.maximum_fee.atoms = Quantity::new("4").unwrap();
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r.quote.maximum_fee.atoms = Quantity::new("2").unwrap();
        r.quote.evidence.observed_at = "2026-01-01T00:03:00Z".into();
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "AMM_QUOTE_STALE");
        r.quote.evidence.observed_at = "2025-12-31T23:58:59Z".into();
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "AMM_QUOTE_STALE");
    }
    #[test]
    fn rejects_input_slippage_and_route_scope_mismatch() {
        let now = Utc::now();
        let mut r = request();
        r.selected_slippage_bps = 250;
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r.selected_slippage_bps = 100;
        r.quote.input.atoms = Quantity::new("111").unwrap();
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r.quote.input.atoms = Quantity::new("101").unwrap();
        r.quote.route.assets[1] = asset("different-asset");
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
    }

    #[test]
    fn rejects_direct_math_underflow_zero_rate_and_impossible_floor() {
        let mut r = request();
        r.selected_slippage_bps = 10_000;
        assert_eq!(minimum_output(&r).unwrap(), Quantity::zero());
        r.selected_slippage_bps = 10_001;
        assert!(minimum_output(&r).is_err());
        r.selected_slippage_bps = 100;
        r.rate_floor = Some(RateFloor {
            output_atoms: Quantity::zero(),
            input_atoms: Quantity::new("1").unwrap(),
        });
        assert!(minimum_output(&r).is_err());
        r.rate_floor = None;
        r.absolute_minimum_output = Some(Quantity::new("100").unwrap());
        assert_eq!(
            validate_swap(
                &r,
                DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
            .unwrap_err()
            .code,
            "AMM_BOUND_EXCEEDED"
        );
    }

    #[test]
    fn enforces_approved_route_resource_network_fee_and_evidence_scope() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut r = request();
        r.quote.route.fingerprint = "different".into();
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r = request();
        r.quote.resource = ResourceId("other-wallet".into());
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r = request();
        r.fee_cap.asset = asset("other-fee");
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r = request();
        r.quote.maximum_fee.asset = asset("other-network:gas");
        r.quote.maximum_fee.asset.network = "elsewhere".into();
        assert_eq!(
            validate_swap(&r, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
        r = request();
        r.quote.evidence.block_hash = None;
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "INVALID_INPUT");
        r = request();
        r.quote.evidence.block_hash = Some("\n".into());
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "INVALID_INPUT");
        r = request();
        r.quote.price_impact_bps = 10_001;
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "INVALID_INPUT");
    }

    #[test]
    fn accepts_exact_age_boundary_and_rejects_fractional_future_or_expiry() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:02:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut r = request();
        assert!(validate_swap(&r, now).is_ok());
        r.quote.evidence.observed_at = "2025-12-31T23:59:59.999Z".into();
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "AMM_QUOTE_STALE");
        r.quote.evidence.observed_at = "2026-01-01T00:02:00.001Z".into();
        assert_eq!(validate_swap(&r, now).unwrap_err().code, "AMM_QUOTE_STALE");
    }

    #[test]
    fn refreshed_quote_cannot_expand_price_impact_scope() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let r = request();
        let mut fresh = r.quote.clone();
        fresh.price_impact_bps = 51;
        assert_eq!(
            validate_refreshed_quote(&r, &fresh, now).unwrap_err().code,
            "AMM_BOUND_EXCEEDED"
        );
    }
}
