//! Immutable approval ceilings, separate from refreshed pricing evidence.
use crate::product_names::MetadataValueExt;
use crate::{AssetId, GoFinError, PreparedOperation, Quantity, Result};
#[cfg(test)]
use chrono::Duration;
use chrono::{DateTime, Utc};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

pub const KEY: &str = "_gofin_usd_ceiling_v1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceIdentity {
    asset: AssetId,
    unit_atoms: Quantity,
    source: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ceiling {
    movement_usd_micros: Quantity,
    maximum_fee_usd_micros: Quantity,
    additional_allocation_usd_micros: Quantity,
    prices: Vec<PriceIdentity>,
}

fn identity(source: &str) -> String {
    source
        .split(';')
        .filter(|part| {
            let part = part.trim();
            let timestamp = part
                .strip_prefix("updated:")
                .or_else(|| part.strip_prefix("identity_observed_at="));
            !timestamp.is_some_and(|value| DateTime::parse_from_rfc3339(value).is_ok())
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("; ")
}

fn cents_up(value: &Quantity) -> Quantity {
    let cent = BigUint::from(10_000u32);
    let rounded = ((value.integer() + &cent - BigUint::from(1u8)) / &cent) * cent;
    Quantity::new(rounded.to_string()).unwrap_or_else(|_| value.clone())
}

fn usd(value: &Quantity) -> String {
    let micros = value.integer();
    let scale = BigUint::from(1_000_000u32);
    format!("{}.{:06}", &micros / &scale, &micros % scale)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

pub fn bind(prepared: &mut PreparedOperation) {
    bind_with_margin(prepared, None);
}

/// Reserve a finite 1% USD price buffer before approval/attempt binding.
/// This changes accounting ceilings, never asset amounts, fee units or prices.
/// Existing bound records retain their original ceilings through `carry_at`.
pub fn bind_with_price_buffer(prepared: &mut PreparedOperation) -> Result<()> {
    let Some(value) = prepared.valuation.as_ref() else {
        return Ok(());
    };
    let ceiling = |amount: &Quantity| {
        // Round upward once, to a full cent. Reject unrepresentable ceilings
        // before mutating the candidate; zero receives no extra allowance.
        let cent = BigUint::from(10_000u32);
        let divisor = &cent * 100u32;
        let rounded = ((amount.integer() * 101u32 + &divisor - 1u32) / divisor) * cent;
        Quantity::new(rounded.to_string())
    };
    let ceilings = [
        ceiling(&value.movement_usd_micros)?,
        ceiling(&value.maximum_fee_usd_micros)?,
        ceiling(&value.additional_allocation_usd_micros)?,
    ];
    bind_with_margin(prepared, Some(ceilings));
    Ok(())
}

fn bind_with_margin(prepared: &mut PreparedOperation, buffered: Option<[Quantity; 3]>) {
    let (Some(value), Some(binding)) = (
        prepared.valuation.as_mut(),
        prepared.binding.as_object_mut(),
    ) else {
        return;
    };
    let price_buffer = buffered.is_some();
    let [movement, fee, allocation] = buffered.unwrap_or_else(|| {
        [
            cents_up(&value.movement_usd_micros),
            cents_up(&value.maximum_fee_usd_micros),
            cents_up(&value.additional_allocation_usd_micros),
        ]
    });
    let bounds = Ceiling {
        movement_usd_micros: movement,
        maximum_fee_usd_micros: fee,
        additional_allocation_usd_micros: allocation,
        prices: value
            .prices
            .iter()
            .map(|price| PriceIdentity {
                asset: price.asset.clone(),
                unit_atoms: price.unit_atoms.clone(),
                source: identity(&price.source),
            })
            .collect(),
    };
    if prepared
        .intent
        .maximum_costs
        .iter()
        .any(|cost| cost.treatment == crate::CostTreatment::VenueCalculatedAdditionalDebit)
    {
        prepared.summary.push_str(&format!(
            " USD ceilings: ${} principal; ${} upfront fee reserve (not a fee cap).",
            usd(&bounds.movement_usd_micros),
            usd(&bounds.maximum_fee_usd_micros)
        ));
    } else {
        prepared.summary.push_str(&format!(
            " USD ceilings: ${} movement, ${} fees.",
            usd(&bounds.movement_usd_micros),
            usd(&bounds.maximum_fee_usd_micros)
        ));
    }
    if price_buffer {
        prepared.summary.push_str(" USD ceilings include a 1% pricing buffer, rounded up to cents; asset and network-fee limits are unchanged.");
    }
    value.movement_usd_micros = bounds.movement_usd_micros.clone();
    value.maximum_fee_usd_micros = bounds.maximum_fee_usd_micros.clone();
    value.additional_allocation_usd_micros = bounds.additional_allocation_usd_micros.clone();
    binding.insert(KEY.into(), serde_json::json!(bounds));
}

fn invalid() -> GoFinError {
    GoFinError::new(
        "VALUE_BOUND_EXCEEDED",
        "valuation",
        "Fresh valuation exceeds the approved USD ceiling or changes a priced asset identity.",
        "Refresh the preview and obtain fresh approval; nothing was submitted.",
    )
}

pub fn validate(prepared: &PreparedOperation) -> Result<()> {
    validate_at(prepared, Utc::now())
}

pub fn validate_at(prepared: &PreparedOperation, at: chrono::DateTime<Utc>) -> Result<()> {
    check(prepared, true, at)
}

fn check(prepared: &PreparedOperation, fresh: bool, at: chrono::DateTime<Utc>) -> Result<()> {
    let Some(raw) = prepared.binding.product_get(KEY)? else {
        return Ok(());
    };
    let bounds: Ceiling = serde_json::from_value(raw.clone()).map_err(|_| invalid())?;
    let value = prepared.valuation.as_ref().ok_or_else(invalid)?;
    if bounds.prices.is_empty() {
        return Err(invalid());
    }
    for expected in &bounds.prices {
        if bounds
            .prices
            .iter()
            .filter(|price| price.asset == expected.asset)
            .count()
            != 1
        {
            return Err(invalid());
        }
        let rows: Vec<_> = value
            .prices
            .iter()
            .filter(|price| price.asset == expected.asset)
            .collect();
        let [price] = rows.as_slice() else {
            return Err(invalid());
        };
        if price.unit_atoms != expected.unit_atoms || identity(&price.source) != expected.source {
            return Err(invalid());
        }
    }
    // Check identities before numeric ceilings: a price increase must never
    // disguise a changed asset, unit or provider as recoverable price drift.
    for (field, actual, maximum) in [
        (
            "movement_usd_micros",
            &value.movement_usd_micros,
            &bounds.movement_usd_micros,
        ),
        (
            "maximum_fee_usd_micros",
            &value.maximum_fee_usd_micros,
            &bounds.maximum_fee_usd_micros,
        ),
        (
            "additional_allocation_usd_micros",
            &value.additional_allocation_usd_micros,
            &bounds.additional_allocation_usd_micros,
        ),
    ] {
        if actual.integer() > maximum.integer() {
            return Err(invalid().with_details(serde_json::json!({
                "reason":"usd_ceiling_exceeded", "field":field,
                "observed_usd_micros":actual, "maximum_usd_micros":maximum
            })));
        }
    }
    for price in value.prices.iter().filter(|_| fresh) {
        let failure =
            crate::price_freshness::failure_details(&price.observed_at, &price.source, at);
        let invalid_price =
            price.usd_micros_per_unit.is_zero() && !crate::unpriced::is_supported_unpriced(price);
        if failure.is_some() || price.source.is_empty() || invalid_price {
            let mut details = failure.unwrap_or_else(|| serde_json::json!({
                "reason": if price.source.is_empty() { "missing_price_source" } else { "zero_price" },
                "observed_at": price.observed_at,
                "checked_at": at.to_rfc3339(),
            }));
            details["asset"] = serde_json::json!(price.asset);
            details["source"] = serde_json::json!(price.source);
            return Err(GoFinError::new(
                "STALE_VALUATION",
                "valuation",
                "USD pricing evidence is invalid or expired.",
                "Obtain fresh pricing and a new preview before approval.",
            )
            .with_details(details));
        }
    }
    Ok(())
}

/// Retain approved ceilings/reservations and actual fresh quotes separately.
pub fn carry(approved: &PreparedOperation, refreshed: &mut PreparedOperation) -> Result<()> {
    carry_at(approved, refreshed, Utc::now())
}

pub fn carry_at(
    approved: &PreparedOperation,
    refreshed: &mut PreparedOperation,
    at: chrono::DateTime<Utc>,
) -> Result<()> {
    let Some(raw) = approved.binding.product_get(KEY)? else {
        return Ok(());
    };
    // Original quote times can expire while consent is pending; their shape and
    // identities must still be intact. Only the refreshed evidence may settle.
    check(approved, false, at)?;
    refreshed.binding[KEY] = raw.clone();
    validate_at(refreshed, at)?;
    let bounds: Ceiling = serde_json::from_value(raw.clone()).map_err(|_| invalid())?;
    let value = refreshed.valuation.as_mut().ok_or_else(invalid)?;
    value.movement_usd_micros = bounds.movement_usd_micros;
    value.maximum_fee_usd_micros = bounds.maximum_fee_usd_micros;
    value.additional_allocation_usd_micros = bounds.additional_allocation_usd_micros;
    refreshed.summary = approved.summary.clone();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AccountingLocation, Amount, AssetId, AssetPrice, CostBound, CostTreatment, FinancialIntent,
        ResourceId, Valuation, authorization_binding, binding_hash, now,
    };
    use serde_json::json;

    fn fixture(micros: &str) -> PreparedOperation {
        PreparedOperation {
            operation: "synthetic.transfer".into(),
            operation_version: 1,
            action_deadline: None,
            input: json!({"amount":"1"}),
            binding: json!({"amount":"1","destination":"SYNTHETIC-wallet"}),
            summary: "Send 1 SYNTHETIC to SYNTHETIC-wallet.".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (Utc::now() + Duration::seconds(60)).to_rfc3339(),
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![],
            },
            valuation: Some(Valuation {
                movement_usd_micros: Quantity::new(micros).unwrap(),
                maximum_fee_usd_micros: Quantity::new("1001").unwrap(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: now(),
                source: "synthetic feed".into(),
                prices: vec![AssetPrice {
                    asset: AssetId {
                        network: "ethereum:1".into(),
                        reference: "0x1111111111111111111111111111111111111111".into(),
                        decimals: 18,
                    },
                    unit_atoms: Quantity::from_decimal("1", 18).unwrap(),
                    usd_micros_per_unit: Quantity::new(micros).unwrap(),
                    observed_at: now(),
                    source: "synthetic provider; coin_id=SYNTHETIC; updated:2026-01-01T00:00:00Z"
                        .into(),
                }],
            }),
        }
    }

    #[test]
    fn venue_calculated_cost_summary_calls_usd_fee_amount_a_reserve_not_a_cap() {
        let mut prepared = fixture("1000000");
        prepared.intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("coinbase:SYNTHETIC-account".into()),
            },
            amount: Amount {
                asset: AssetId {
                    network: "coinbase".into(),
                    reference: "USDC".into(),
                    decimals: 36,
                },
                atoms: Quantity::new("100000000000000000").unwrap(),
            },
            treatment: CostTreatment::VenueCalculatedAdditionalDebit,
        });
        bind(&mut prepared);
        assert!(prepared.summary.contains("upfront fee reserve"));
        assert!(prepared.summary.contains("not a fee cap"));
    }

    #[test]
    fn existing_intents_keep_their_cost_treatment_encoding_and_new_policy_round_trips() {
        let legacy = json!({
            "debits":[],
            "maximum_costs":[{"location":{"kind":"cash","resource":"coinbase:SYNTHETIC-account"},
                "amount":{"asset":{"network":"coinbase","reference":"USDC","decimals":36},"atoms":"100"},
                "treatment":"additional_debit"}],
            "expected":[],"locks":[]
        });
        let legacy_intent: FinancialIntent = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&legacy_intent).unwrap()["maximum_costs"][0]["treatment"],
            "additional_debit"
        );

        let mut prepared = fixture("1000000");
        prepared.intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("coinbase:SYNTHETIC-account".into()),
            },
            amount: Amount {
                asset: AssetId {
                    network: "coinbase".into(),
                    reference: "USDC".into(),
                    decimals: 36,
                },
                atoms: Quantity::new("100").unwrap(),
            },
            treatment: CostTreatment::VenueCalculatedAdditionalDebit,
        });
        let encoded = serde_json::to_value(&prepared).unwrap();
        assert_eq!(
            encoded["intent"]["maximum_costs"][0]["treatment"],
            "venue_calculated_additional_debit"
        );
        let decoded: PreparedOperation = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, prepared);
    }

    #[test]
    fn legacy_ceiling_carries_without_rewriting_approval_or_weakening_alias_checks() {
        let mut approved = fixture("1000000");
        bind(&mut approved);
        let ceiling = approved
            .binding
            .as_object_mut()
            .unwrap()
            .remove(KEY)
            .unwrap();
        approved.binding["_fin_usd_ceiling_v1"] = ceiling.clone();
        let original = approved.clone();
        let hash = binding_hash(&authorization_binding(&approved)).unwrap();
        validate(&approved).unwrap();
        let mut refreshed = fixture("1000000");
        carry(&approved, &mut refreshed).unwrap();
        crate::product_names::preserve_approved_spelling(&approved.binding, &mut refreshed.binding)
            .unwrap();
        assert_eq!(
            binding_hash(&authorization_binding(&refreshed)).unwrap(),
            hash
        );
        assert_eq!(approved, original);
        approved.binding[KEY] = ceiling;
        assert!(validate(&approved).is_err());
        assert!(binding_hash(&authorization_binding(&approved)).is_err());
    }

    #[test]
    fn buffered_ceilings_reserve_drift_without_changing_quantities_prices_or_legacy_caps() {
        let mut approved = fixture("25329948");
        let original = approved.clone();
        bind_with_price_buffer(&mut approved).unwrap();
        assert_eq!(approved.binding[KEY]["movement_usd_micros"], "25590000");
        assert_eq!(
            approved
                .valuation
                .as_ref()
                .unwrap()
                .movement_usd_micros
                .as_str(),
            "25590000"
        );
        assert_eq!(approved.intent, original.intent);
        assert_eq!(approved.input, original.input);
        assert_eq!(
            approved.valuation.as_ref().unwrap().prices,
            original.valuation.as_ref().unwrap().prices
        );
        assert!(approved.summary.contains("1% pricing buffer"));
        let hash = binding_hash(&authorization_binding(&approved)).unwrap();
        for current in ["25330001", "25583248", "25590000"] {
            let mut refreshed = fixture(current);
            carry(&approved, &mut refreshed).unwrap();
            assert_eq!(
                binding_hash(&authorization_binding(&refreshed)).unwrap(),
                hash
            );
            assert_eq!(
                refreshed
                    .valuation
                    .as_ref()
                    .unwrap()
                    .movement_usd_micros
                    .as_str(),
                "25590000"
            );
            assert_eq!(
                refreshed.valuation.as_ref().unwrap().prices[0]
                    .usd_micros_per_unit
                    .as_str(),
                current
            );
        }
        let error = carry(&approved, &mut fixture("25590001")).unwrap_err();
        assert_eq!(error.code, "VALUE_BOUND_EXCEEDED");
        assert_eq!(error.details.unwrap()["field"], "movement_usd_micros");
        for invalid in ["stale", "identity"] {
            let mut refreshed = fixture("25330001");
            let price = &mut refreshed.valuation.as_mut().unwrap().prices[0];
            if invalid == "stale" {
                price.observed_at = "2000-01-01T00:00:00Z".into();
            } else {
                price.asset.reference = "SYNTHETIC-OTHER".into();
            }
            assert!(carry(&approved, &mut refreshed).is_err(), "{invalid}");
        }
        let mut legacy = fixture("25329948");
        bind(&mut legacy);
        assert_eq!(legacy.binding[KEY]["movement_usd_micros"], "25330000");
        assert!(carry(&legacy, &mut fixture("25330001")).is_err());
        let mut zero = fixture("0");
        bind_with_price_buffer(&mut zero).unwrap();
        assert_eq!(zero.binding[KEY]["movement_usd_micros"], "0");
        assert_eq!(zero.binding[KEY]["additional_allocation_usd_micros"], "0");
    }

    #[test]
    fn unrepresentable_buffer_rejects_without_mutating_candidate() {
        let mut prepared = fixture(&"9".repeat(256));
        let before = serde_json::to_value(&prepared).unwrap();
        assert!(bind_with_price_buffer(&mut prepared).is_err());
        assert_eq!(serde_json::to_value(&prepared).unwrap(), before);
    }

    #[test]
    fn refreshed_evidence_within_explicit_ceiling_keeps_binding_and_reservation() {
        let mut approved = fixture("1000001");
        bind(&mut approved);
        assert_eq!(
            approved
                .valuation
                .as_ref()
                .unwrap()
                .movement_usd_micros
                .as_str(),
            "1010000"
        );
        assert!(
            approved
                .summary
                .ends_with("USD ceilings: $1.01 movement, $0.01 fees.")
        );
        let hash = binding_hash(&authorization_binding(&approved)).unwrap();
        // Original evidence can age out while approval is pending. Do not reset
        // its timestamp; retain fresh replacement evidence and the old ceiling.
        approved.valuation.as_mut().unwrap().prices[0].observed_at = "2000-01-01T00:00:00Z".into();
        let mut refreshed = fixture("1005000");
        bind(&mut refreshed);
        refreshed.valuation.as_mut().unwrap().prices[0].source =
            "synthetic provider; coin_id=SYNTHETIC; updated:2026-01-01T00:01:00Z".into();
        carry(&approved, &mut refreshed).unwrap();
        assert_eq!(
            hash,
            binding_hash(&authorization_binding(&refreshed)).unwrap()
        );
        assert_eq!(
            approved.valuation.as_ref().unwrap().movement_usd_micros,
            refreshed.valuation.as_ref().unwrap().movement_usd_micros
        );
        assert_eq!(
            refreshed.valuation.as_ref().unwrap().prices[0]
                .usd_micros_per_unit
                .as_str(),
            "1005000"
        );
        assert_ne!(
            refreshed.valuation.as_ref().unwrap().prices[0].observed_at,
            "2000-01-01T00:00:00Z"
        );
        refreshed.binding["destination"] = json!("SYNTHETIC-other");
        assert_ne!(
            hash,
            binding_hash(&authorization_binding(&refreshed)).unwrap()
        );
    }

    #[test]
    fn stale_valuation_identifies_the_expired_asset_in_multi_asset_evidence() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut prepared = fixture("1000000");
        let prices = &mut prepared.valuation.as_mut().unwrap().prices;
        prices[0].observed_at = now.to_rfc3339();
        let mut stale = prices[0].clone();
        stale.asset.reference = "native".into();
        stale.observed_at = (now - Duration::seconds(121)).to_rfc3339();
        let stale_asset = stale.asset.clone();
        prices.push(stale);
        bind(&mut prepared);
        let error = validate_at(&prepared, now).unwrap_err();
        assert_eq!(error.code, "STALE_VALUATION");
        let details = error.details.unwrap();
        assert_eq!(details["asset"], json!(stale_asset));
        assert_eq!(details["checked_at"], now.to_rfc3339());
        assert_eq!(details["evidence"][0]["age_ms"], 121_000);
        assert_eq!(details["evidence"][0]["maximum_age_ms"], 120_000);
        assert_eq!(
            prepared.valuation.as_ref().unwrap().prices[1].observed_at,
            (now - Duration::seconds(121)).to_rfc3339(),
            "diagnostics must not restamp evidence"
        );
    }

    #[test]
    fn higher_costs_identity_changes_duplicate_and_expired_evidence_are_rejected() {
        let mut approved = fixture("1000001");
        bind(&mut approved);
        for change in [
            "movement",
            "fee",
            "allocation",
            "contract",
            "network",
            "unit",
            "source",
            "duplicate",
            "stale",
            "future",
        ] {
            let mut refreshed = fixture("1005000");
            let value = refreshed.valuation.as_mut().unwrap();
            match change {
                "movement" => value.movement_usd_micros = Quantity::new("1010001").unwrap(),
                "fee" => value.maximum_fee_usd_micros = Quantity::new("10001").unwrap(),
                "allocation" => {
                    value.additional_allocation_usd_micros = Quantity::new("1").unwrap()
                }
                "contract" => {
                    value.prices[0].asset.reference =
                        "0x2222222222222222222222222222222222222222".into()
                }
                "network" => value.prices[0].asset.network = "ethereum:42161".into(),
                "unit" => value.prices[0].unit_atoms = Quantity::new("1").unwrap(),
                "source" => value.prices[0].source = "synthetic provider; coin_id=OTHER".into(),
                "duplicate" => value.prices.push(value.prices[0].clone()),
                "stale" => value.prices[0].observed_at = "2000-01-01T00:00:00Z".into(),
                _ => value.prices[0].observed_at = (Utc::now() + Duration::seconds(1)).to_rfc3339(),
            }
            assert!(carry(&approved, &mut refreshed).is_err(), "{change}");
        }
        let mut corrupted = approved.clone();
        corrupted.binding[KEY]["unexpected"] = json!(true);
        assert!(validate(&corrupted).is_err());
    }

    #[test]
    fn numeric_ceiling_details_require_valid_price_identities_and_bounds() {
        let mut approved = fixture("1000000");
        bind(&mut approved);
        for change in ["none", "asset", "unit", "source", "duplicate", "missing"] {
            // The same one-cent increase accompanies each identity violation.
            let mut refreshed = fixture("1010000");
            let prices = &mut refreshed.valuation.as_mut().unwrap().prices;
            match change {
                "asset" => prices[0].asset.reference = "SYNTHETIC-OTHER".into(),
                "unit" => prices[0].unit_atoms = Quantity::new("1").unwrap(),
                "source" => prices[0].source = "SYNTHETIC-OTHER".into(),
                "duplicate" => prices.push(prices[0].clone()),
                "missing" => prices.clear(),
                _ => {}
            }
            let error = carry(&approved, &mut refreshed).unwrap_err();
            assert_eq!(error.code, "VALUE_BOUND_EXCEEDED");
            if change == "none" {
                let details = error.details.unwrap();
                assert_eq!(details["reason"], "usd_ceiling_exceeded");
                assert_eq!(details["observed_usd_micros"], "1010000");
                assert_eq!(details["maximum_usd_micros"], "1000000");
            } else {
                assert!(
                    error.details.is_none(),
                    "{change} must not qualify for refresh"
                );
            }
        }
        for change in ["missing", "malformed", "empty_prices", "duplicate_prices"] {
            let mut invalid = approved.clone();
            invalid.valuation.as_mut().unwrap().movement_usd_micros =
                Quantity::new("1010000").unwrap();
            match change {
                "missing" => invalid.valuation = None,
                "malformed" => invalid.binding[KEY]["movement_usd_micros"] = json!(null),
                "empty_prices" => invalid.binding[KEY]["prices"] = json!([]),
                _ => {
                    let price = invalid.binding[KEY]["prices"][0].clone();
                    invalid.binding[KEY]["prices"] = json!([price, price]);
                }
            }
            let error = validate(&invalid).unwrap_err();
            assert_eq!(error.code, "VALUE_BOUND_EXCEEDED");
            assert!(
                error.details.is_none(),
                "{change} must not qualify for refresh"
            );
        }
    }

    #[test]
    fn legacy_exact_quote_binding_is_unchanged_and_cent_rounding_never_rounds_down() {
        for (input, expected) in [
            ("0", "0"),
            ("1", "10000"),
            ("9999", "10000"),
            ("10000", "10000"),
            ("10001", "20000"),
        ] {
            assert_eq!(cents_up(&Quantity::new(input).unwrap()).as_str(), expected);
        }
        let prepared = fixture("1000001");
        assert_eq!(
            authorization_binding(&prepared)["value_bounds"]["movement_usd_micros"],
            json!("1000001")
        );
        assert!(
            authorization_binding(&prepared)["value_bounds"]["prices"][0]
                .get("usd_micros_per_unit")
                .is_some()
        );
        assert_eq!(
            identity("synthetic; coin_id=X; updated:invalid"),
            "synthetic; coin_id=X; updated:invalid"
        );
    }
    #[test]
    fn explicit_unknown_estimate_keeps_exact_bound_and_cannot_change_price_status() {
        let mut unknown = fixture("0");
        let value = unknown.valuation.as_mut().unwrap();
        let row = &mut value.prices[0];
        row.asset.network = "binance".into();
        row.asset.reference = "APT".into();
        row.source = crate::unpriced::source(&row.asset);
        bind(&mut unknown);
        validate(&unknown).unwrap();
        let mut refreshed = unknown.clone();
        refreshed.binding.as_object_mut().unwrap().remove(KEY);
        carry(&unknown, &mut refreshed).unwrap();
        for field in ["price", "source", "unit", "network"] {
            let mut changed = unknown.clone();
            changed.binding.as_object_mut().unwrap().remove(KEY);
            let row = &mut changed.valuation.as_mut().unwrap().prices[0];
            match field {
                "price" => {
                    row.usd_micros_per_unit = Quantity::from_u64(1);
                    row.source = "verified synthetic price".into();
                }
                "source" => row.source.push_str("; different"),
                "unit" => row.unit_atoms = Quantity::from_u64(1),
                _ => row.asset.network = "ethereum:1".into(),
            }
            assert!(carry(&unknown, &mut changed).is_err(), "{field}");
        }
        let mut priced = unknown.clone();
        priced.binding.as_object_mut().unwrap().remove(KEY);
        let row = &mut priced.valuation.as_mut().unwrap().prices[0];
        row.usd_micros_per_unit = Quantity::from_u64(1);
        row.source = "verified synthetic price".into();
        bind(&mut priced);
        let mut refreshed_unknown = unknown.clone();
        assert!(carry(&priced, &mut refreshed_unknown).is_err());
    }
}
