//! Editing one native member revises its original batch's entire cumulative authority.
use crate::{
    KrakenEndpoint as E, contracts,
    profile::{KrakenProfile, error},
    trading::controls,
};
use gofin_domain::amendments::AmendmentScope;
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{
    Amount, EffectConstraint, FinancialIntent, PositionKind, PreparedOperation, Quantity,
    ResourceAmount, Result, SignedQuantity,
};
use serde_json::{Value, json};
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_batch_member_edit(
        &self,
        input: &Value,
        previous: &AmendmentScope,
    ) -> Result<PreparedOperation> {
        let (_, native) = contracts::decode(E::KrakenfuturesPrivatePostEditorder, input)?;
        let account = self.account(
            input["account"]
                .as_str()
                .ok_or_else(|| error("INVALID_INPUT", "Exact derivative account required."))?,
        )?;
        let (index, binding) = self
            .derivative_original_member(&previous.binding, &native, account)
            .await?;
        let resource = gofin_domain::ResourceId(format!(
            "{}:wallet:{}",
            account.resource.0,
            binding["derivative"]["wallet"].as_str().unwrap()
        ));
        let mut intent = previous.intent.clone();
        intent.debits.retain(|d| d.resource == resource);
        intent.maximum_costs.retain(|c| {
            c.location
                == gofin_domain::AccountingLocation::Cash {
                    resource: resource.clone(),
                }
        });
        intent.expected = member_constraints(&binding)?;
        let virtual_previous = AmendmentScope {
            intent,
            binding: binding.clone(),
            valuation: previous.valuation.clone(),
        };
        let mut edit =
            Box::pin(self.prepare_derivative_edit(input, Some(&virtual_previous))).await?;
        let revision = edit
            .intent
            .expected
            .iter()
            .find_map(|e| match e {
                EffectConstraint::SubmissionAmendment { revision, .. } => {
                    Some((**revision).clone())
                }
                _ => None,
            })
            .ok_or_else(|| error("KRAKEN_PREVIEW_INVALID", "Exact member revision missing."))?;
        let mut full = previous.clone();
        full.intent.debits.retain(|d| d.resource != resource);
        full.intent.debits.extend(revision.intent.debits.clone());
        full.intent.maximum_costs.retain(|c| {
            c.location
                != gofin_domain::AccountingLocation::Cash {
                    resource: resource.clone(),
                }
        });
        full.intent
            .maximum_costs
            .extend(revision.intent.maximum_costs.clone());
        full.intent
            .expected
            .retain(|e| !matches!(e,EffectConstraint::Settlement{resource:r,..} if r==&resource));
        full.intent.expected.extend(
            revision
                .intent
                .expected
                .iter()
                .filter(|e| matches!(e, EffectConstraint::Settlement { .. }))
                .cloned(),
        );
        full.binding["derivativeBatch"][index] = revision.binding.clone();
        let tag = full.binding["parameters"]["batchOrder"][index]["order_tag"].clone();
        let mut row = revision.binding["parameters"].clone();
        row["order"] = json!("send");
        row["order_tag"] = tag;
        full.binding["parameters"]["batchOrder"][index] = row;
        if let Some(fees) = revision
            .binding
            .get("parameters")
            .and_then(|_| input["maximumFees"].as_object())
        {
            for (code, cap) in fees {
                full.binding["controls"]["maximumFees"][code] = cap.clone();
            }
        }
        let mut prepared = Vec::new();
        for b in full.binding["derivativeBatch"].as_array().unwrap() {
            if b["derivative"].is_object() {
                let mut p = edit.clone();
                p.binding = b.clone();
                p.intent.expected = member_constraints(b)?;
                prepared.push(p);
            }
        }
        crate::derivative_groups::collate(&prepared, &mut full.intent)?;
        // Original native observation remains the baseline for all cumulative member fills.
        full.binding[gofin_domain::BASELINE_ANCHORS_KEY] = previous
            .binding
            .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
            .clone();
        full.valuation = Some(self.batch_exposure_valuation(&full).await?);
        let positive: Quantity = serde_json::from_value(
            edit.binding["positiveExposureUsdMicros"].clone(),
        )
        .map_err(|_| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Independent positive member exposure valuation missing.",
            )
        })?;
        let old_value = previous.valuation.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Original full batch USD scope missing.",
            )
        })?;
        let new_value = full.valuation.as_mut().unwrap();
        new_value.movement_usd_micros = new_value
            .movement_usd_micros
            .clone()
            .max(old_value.movement_usd_micros.checked_add(&positive)?);
        new_value.maximum_fee_usd_micros = new_value.maximum_fee_usd_micros.clone().max(
            old_value
                .maximum_fee_usd_micros
                .checked_add(&edit.valuation.as_ref().unwrap().maximum_fee_usd_micros)?,
        );
        let mut priced_scope = edit.clone();
        priced_scope.intent = full.intent.clone();
        priced_scope.binding = full.binding.clone();
        priced_scope.valuation = full.valuation.clone();
        gofin_domain::value_bounds::bind(&mut priced_scope);
        full.binding = priced_scope.binding;
        let mut delta = FinancialIntent {
            locks: full.intent.locks.clone(),
            ..Default::default()
        };
        for amount in &full.intent.debits {
            let old = previous
                .intent
                .debits
                .iter()
                .filter(|d| d.resource == amount.resource && d.amount.asset == amount.amount.asset)
                .try_fold(Quantity::zero(), |n, d| n.checked_add(&d.amount.atoms))?;
            if amount.amount.atoms > old {
                let mut a = amount.clone();
                a.amount.atoms = amount.amount.atoms.checked_sub(&old)?;
                delta.debits.push(a);
            }
        }
        for amount in &full.intent.maximum_costs {
            let old = previous
                .intent
                .maximum_costs
                .iter()
                .filter(|d| {
                    d.location == amount.location
                        && d.amount.asset == amount.amount.asset
                        && d.treatment == amount.treatment
                })
                .try_fold(Quantity::zero(), |n, d| n.checked_add(&d.amount.atoms))?;
            if amount.amount.atoms > old {
                let mut a = amount.clone();
                a.amount.atoms = amount.amount.atoms.checked_sub(&old)?;
                delta.maximum_costs.push(a);
            }
        }
        let mut value = self
            .financial_valuation(&delta.debits, &delta.maximum_costs)
            .await?;
        let total = |v: &gofin_domain::Valuation| -> Result<Quantity> {
            v.movement_usd_micros
                .checked_add(&v.maximum_fee_usd_micros)?
                .checked_add(&v.additional_allocation_usd_micros)
        };
        let old = total(previous.valuation.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Original full batch valuation missing.",
            )
        })?)?;
        let new = total(full.valuation.as_ref().unwrap())?;
        if new > old {
            let required = new.checked_sub(&old)?;
            let own = total(&value)?;
            if required > own {
                value.additional_allocation_usd_micros = value
                    .additional_allocation_usd_micros
                    .checked_add(&required.checked_sub(&own)?)?;
            }
        }
        let c = controls(input)?;
        delta.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: c.operation_id.unwrap(),
            submission: c.original_submission.unwrap(),
            revision: Box::new(full),
        });
        edit.intent = delta;
        edit.valuation = Some(value);
        edit.binding["amendmentPreviousScope"] = json!(previous);
        edit.binding["originalBatchMemberIndex"] = json!(index);
        gofin_domain::value_bounds::bind(&mut edit);
        Ok(edit)
    }
    async fn batch_exposure_valuation(
        &self,
        scope: &AmendmentScope,
    ) -> Result<gofin_domain::Valuation> {
        let instruments = self
            .json(E::KrakenfuturesPublicGetInstruments, None, json!({}))
            .await?;
        let mut notionals = Vec::new();
        for b in scope.binding["derivativeBatch"].as_array().unwrap() {
            if !b["derivative"].is_object() {
                continue;
            }
            let d = &b["derivative"];
            let i = instruments["instruments"]
                .as_array()
                .and_then(|rows| rows.iter().find(|i| i["symbol"] == d["symbol"]))
                .ok_or_else(|| {
                    error(
                        "KRAKEN_INSTRUMENT_UNKNOWN",
                        "Exact original contract metadata missing.",
                    )
                })?;
            let size: Quantity = serde_json::from_value(d["size"].clone())
                .map_err(|_| error("KRAKEN_PREVIEW_INVALID", "Original contract size missing."))?;
            let units = size.mul_div_ceil(
                &crate::observation::atoms(&i["contractSize"])?,
                &Quantity::from_decimal("1", 36)?,
            )?;
            let code = match i["type"].as_str() {
                Some("futures_inverse") => "USD",
                Some("futures_vanilla" | "flexible_futures") => {
                    i["base"].as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_CONTRACT_IDENTITY_UNKNOWN",
                            "Exact native base denomination missing.",
                        )
                    })?
                }
                _ => {
                    return Err(error(
                        "KRAKEN_CONTRACT_SEMANTICS_PENDING",
                        "Contract notional denomination unreviewed.",
                    ));
                }
            };
            notionals.push(ResourceAmount {
                resource: gofin_domain::ResourceId(
                    scope.binding["account"].as_str().unwrap().into(),
                ),
                amount: Amount {
                    asset: crate::derivatives::asset(code),
                    atoms: units,
                },
            });
        }
        self.financial_valuation(&notionals, &scope.intent.maximum_costs)
            .await
    }
}
fn member_constraints(binding: &Value) -> Result<Vec<EffectConstraint>> {
    let d = &binding["derivative"];
    let size: Quantity = serde_json::from_value(d["size"].clone())
        .map_err(|_| error("KRAKEN_PREVIEW_INVALID", "Original member size missing."))?;
    let account = binding
        .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|r| r["resource"].as_str())
        .ok_or_else(|| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Original member anchor resource missing.",
            )
        })?;
    Ok(
        [PositionKind::DerivativeLong, PositionKind::DerivativeShort]
            .into_iter()
            .map(|kind| EffectConstraint::DerivativePosition {
                resource: gofin_domain::ResourceId(account.into()),
                position: d["symbol"].as_str().unwrap().into(),
                asset: crate::derivatives::contract(d["symbol"].as_str().unwrap()),
                position_kind: kind,
                minimum_delta: SignedQuantity::negative(&size),
                maximum_delta: SignedQuantity::positive(&size),
            })
            .collect(),
    )
}

impl KrakenProfile {
    pub(crate) async fn derivative_original_member(
        &self,
        scope: &Value,
        selector: &Value,
        account: &crate::KrakenAccount,
    ) -> Result<(usize, Value)> {
        let open = self
            .json(
                E::KrakenfuturesPrivateGetOpenorders,
                Some(account),
                json!({}),
            )
            .await?;
        let rows = scope["derivativeBatch"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Protected original batch members missing.",
            )
        })?;
        let selected = rows
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b["derivative"].is_object()
                    && open["openOrders"].as_array().is_some_and(|orders| {
                        orders.iter().any(|o| {
                            o["cliOrdId"] == b["derivative"]["clientId"]
                                && o["symbol"] == b["derivative"]["symbol"]
                                && (b["derivative"]["orderId"].is_null()
                                    || b["derivative"]["orderId"] == o["order_id"])
                                && (selector
                                    .get("orderId")
                                    .or_else(|| selector.get("order_id"))
                                    .is_some_and(|id| *id == o["order_id"])
                                    || selector
                                        .get("cliOrdId")
                                        .is_some_and(|id| *id == o["cliOrdId"]))
                        })
                    })
            })
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "An edit must resolve to exactly one currently open member of the original batch.",
            ));
        }
        Ok((selected[0].0, selected[0].1.clone()))
    }
}
