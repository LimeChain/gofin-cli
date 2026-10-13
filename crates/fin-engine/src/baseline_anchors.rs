//! Broker-observed pre-action position state, bound by the exact preview approval.
use super::*;
fn invalid() -> FinError {
    FinError::new(
        "INVALID_POSITION_BASELINE",
        "validation",
        "The bound position baseline is missing exact intent identity or verified observation evidence.",
        "Refresh the native position preview; client-provided baseline authority is not accepted.",
    )
}
fn allowed(anchor: &PositionAnchor, intent: &FinancialIntent) -> bool {
    super::position_anchor_requirements(intent)
        .iter()
        .any(|requirement| requirement.matches(anchor))
}

fn rows(prepared: &PreparedOperation) -> Result<Vec<PositionAnchor>> {
    if prepared
        .input
        .get(fin_domain::BASELINE_ANCHORS_KEY)
        .is_some()
    {
        return Err(invalid());
    }
    let Some(value) = prepared.binding.get(fin_domain::BASELINE_ANCHORS_KEY) else {
        return Ok(vec![]);
    };
    let rows: Vec<PositionAnchor> = from_value(value.clone()).map_err(|_| invalid())?;
    let observed = parse_time(&prepared.observed_at)?;
    let mut keys = BTreeSet::new();
    for anchor in &rows {
        let at = parse_time(&anchor.observed_at)?;
        if !allowed(anchor, &prepared.intent)
            || anchor.evidence_version == 0
            || !anchor.evidence.as_object().is_some_and(|v| !v.is_empty())
            || at > observed
            || observed.signed_duration_since(at).num_seconds() > MAX_OBSERVATION_AGE_SECONDS
            || (anchor.position_kind.is_derivative() || anchor.position_kind.is_margin())
                && anchor.scaled_receipt.is_some()
            || !keys.insert(position_anchor_key(anchor)?)
        {
            return Err(invalid());
        }
    }
    Ok(rows)
}
pub(super) fn validate(prepared: &PreparedOperation) -> Result<()> {
    rows(prepared).map(|_| ())
}
pub(super) fn merge(
    prepared: &PreparedOperation,
    prior: &PreparationContext,
) -> Result<PreparationContext> {
    let mut result = prior.clone();
    let mut keys = result
        .positions
        .iter()
        .map(position_anchor_key)
        .collect::<Result<BTreeSet<_>>>()?;
    for anchor in rows(prepared)? {
        if keys.insert(position_anchor_key(&anchor)?) {
            result.positions.push(anchor);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_position_zero_baseline_proves_first_mint_rounding_without_inferred_zero() {
        let observed = now();
        let asset = fin_domain::AssetId {
            network: "synthetic".into(),
            reference: "EXACT-UNDERLYING".into(),
            decimals: 6,
        };
        let resource = ResourceId("synthetic:account".into());
        let prior = PositionAnchor {
            resource: resource.clone(),
            position: "floating-loan".into(),
            position_kind: PositionKind::VariableDebt,
            principal: Amount {
                asset: asset.clone(),
                atoms: fin_domain::Quantity::zero(),
            },
            scaled_receipt: Some(fin_domain::Quantity::zero()),
            evidence_version: 1,
            evidence: json!({"native_block":123,"scaled_debt":"0","index":"1000000000000000000000000001"}),
            observed_at: observed.clone(),
        };
        let prepared = PreparedOperation {
            operation: "synthetic.borrow".into(),
            input: json!({}),
            binding: json!({fin_domain::BASELINE_ANCHORS_KEY:[prior]}),
            summary: "Synthetic first indexed loan".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: observed.clone(),
            expires_at: (Utc::now() + chrono::Duration::seconds(90)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent {
                expected: vec![EffectConstraint::OriginMovement {
                    from: AccountingLocation::Position {
                        resource: resource.clone(),
                        position: "floating-loan".into(),
                        position_kind: PositionKind::VariableDebt,
                    },
                    to: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    debit_asset: asset.clone(),
                    credit_asset: asset.clone(),
                    maximum_debit: fin_domain::Quantity::from_u64(1000),
                    minimum_credit: fin_domain::Quantity::from_u64(1000),
                }],
                ..Default::default()
            },
            action_deadline: None,
            operation_version: 1,
        };
        let mut anchor = from_value::<Vec<PositionAnchor>>(
            prepared.binding[fin_domain::BASELINE_ANCHORS_KEY].clone(),
        )
        .unwrap()
        .remove(0);
        anchor.principal.atoms = fin_domain::Quantity::from_u64(1001);
        anchor.scaled_receipt = Some(fin_domain::Quantity::from_u64(1000));
        let verified = VerifiedEffects {
            submission: Submission {
                kind: "synthetic".into(),
                reference: "once".into(),
                data: json!({}),
            },
            effects: vec![
                FinancialEffect::Accrual {
                    resource: resource.clone(),
                    position: "floating-loan".into(),
                    asset: asset.clone(),
                    position_kind: PositionKind::VariableDebt,
                    delta: fin_domain::SignedQuantity::new("1").unwrap(),
                },
                FinancialEffect::DebtChange {
                    resource: resource.clone(),
                    debt: "floating-loan".into(),
                    asset: asset.clone(),
                    delta: fin_domain::SignedQuantity::new("1000").unwrap(),
                    anchor: Some(anchor.clone()),
                },
            ],
            evidence_version: 1,
            evidence: json!({"exact_native_mint":"1000","indexed_backing":"1001"}),
            proof_digest: "synthetic".into(),
            observed_at: observed,
        };
        assert_eq!(
            validate_anchor_transitions(&verified, &PreparationContext::default())
                .unwrap_err()
                .code,
            "PRIOR_POSITION_ANCHOR_REQUIRED"
        );
        let context = merge(&prepared, &PreparationContext::default()).unwrap();
        validate_anchor_transitions(&verified, &context).unwrap();
        let mut forged = prepared.clone();
        forged.input[fin_domain::BASELINE_ANCHORS_KEY] =
            prepared.binding[fin_domain::BASELINE_ANCHORS_KEY].clone();
        assert!(validate(&forged).is_err());
        let mut historic = anchor;
        historic.principal.atoms = fin_domain::Quantity::from_u64(99);
        let context = merge(
            &prepared,
            &PreparationContext {
                positions: vec![historic],
            },
        )
        .unwrap();
        assert!(validate_anchor_transitions(&verified, &context).is_err());
    }
}
