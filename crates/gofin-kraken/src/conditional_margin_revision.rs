//! An issued margin close changes under the original full-loan settlement owner.
use super::*;
use gofin_domain::amendments::{AmendmentScope, OriginalSubmissionEvidence};

impl KrakenProfile {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_issued_margin_child_revision(
        &self,
        endpoint: E,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: &AmendmentScope,
        original_evidence: Option<&OriginalSubmissionEvidence>,
    ) -> Result<PreparedOperation> {
        let mut margin: MarginBinding = serde_json::from_value(previous.binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original margin scopes are unavailable.",
                )
            })?;
        if let Err(unsupported) = check_conditional_margin_revision_target(&margin, &parameters)
            && unsupported.code == "KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED"
        {
            return Err(unsupported);
        }
        let evidence=original_evidence.ok_or_else(||error("KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED","Recover the original margin entry's exact trades, full loan and custody ledgers before revising an issued closing child."))?;
        let submission = controls
            .original_submission
            .as_ref()
            .ok_or_else(|| error("INVALID_INPUT", "Exact originalSubmission is required."))?;
        if evidence.submission.as_ref() != Some(submission) {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "The protected original journal does not identify this exact margin submission.",
            ));
        }
        let bundle=evidence.verified_effects.as_ref().ok_or_else(||error("KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED","Original native margin financial proof is unavailable; recover its original operation first."))?;
        let inventory =
            crate::conditional_spot_edit::conditional_inventory(self, account, submission).await?;
        let target = if let Some(id) = parameters["txid"].as_str() {
            id.to_owned()
        } else {
            let matches = inventory
                .iter()
                .filter(|(_, row)| {
                    row["status"] == "open"
                        && (parameters["cl_ord_id"]
                            .as_str()
                            .is_some_and(|id| row["cl_ord_id"] == id)
                            || parameters["txid"].as_i64().is_some_and(|reference| {
                                row["userref"].as_i64() == Some(reference)
                            }))
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_EXACT_ID_REQUIRED",
                    "Select one exact issued margin child native txid, uniquely verified native client ID, or unique legacy user reference. Parent/template or ambiguous selectors do not identify a child.",
                ));
            }
            matches[0].clone()
        };
        let row = inventory
            .get(&target)
            .filter(|r| r["status"] == "open")
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_NOT_OPEN",
                    "The selected issued margin child is not exactly open.",
                )
            })?;
        if margin
            .orders
            .iter()
            .any(|o| o.conditional_parent.is_none() && o.order_ids.contains(&target))
        {
            check_conditional_margin_revision_target(&margin, &parameters)?;
        }
        let mut selection = None;
        for (index, child) in margin
            .orders
            .iter()
            .enumerate()
            .filter(|(_, o)| o.conditional_parent.is_some())
        {
            let parent = margin
                .orders
                .iter()
                .find(|o| Some(&o.client_id) == child.conditional_parent.as_ref())
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                        "The original closing template has no exact entry.",
                    )
                })?;
            for (parent_id, parent_row) in inventory
                .iter()
                .filter(|(id, _)| parent.order_ids.contains(id))
            {
                if margin_child_belongs(&target, row, parent_id, &previous.binding) {
                    if selection.is_some() {
                        return Err(error(
                            "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                            "The native child maps to multiple original margin entries.",
                        ));
                    }
                    selection =
                        Some((index, parent.clone(), parent_id.clone(), parent_row.clone()));
                }
            }
        }
        let (index, parent, parent_id, parent_row) = selection.ok_or_else(|| {
            error(
                "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                "The issued child lacks the exact native refid of an original margin entry.",
            )
        })?;
        let template = margin.orders[index].clone();
        if !matches!(
            parent_row["status"].as_str(),
            Some("open" | "closed" | "canceled" | "expired")
        ) {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_PARENT_PENDING",
                "An issued margin close needs a trustworthy native parent status and independently proved issued loan.",
            ));
        }
        let mut records = serde_json::Map::new();
        let mut proved_volume = Quantity::zero();
        let mut proved_loan = Quantity::zero();
        let events = bundle.evidence["proofEvents"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "Immutable original margin trade/ledger events are unavailable.",
            )
        })?;
        for id in parent_row["trades"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "Native parent trade identities are unavailable.",
            )
        })? {
            let id = id.as_str().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                    "Parent trade ID is invalid.",
                )
            })?;
            let proof=events.iter().find(|e|e["eventId"]==format!("kraken:margin-trade:{id}")).ok_or_else(||error("KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED","Recover the original parent trade and its exact custody ledgers before child revision."))?;
            let trade = &proof["trade"];
            if trade["ordertxid"] != parent_id
                || trade["pair"] != parent.pair
                || trade["type"] != if parent.buy { "buy" } else { "sell" }
                || proof["ledgers"].as_object().is_none_or(|r| r.is_empty())
                || atoms(&trade["margin"])?.is_zero()
            {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED",
                    "The immutable native parent proof changed market, direction, loan or ledger attribution.",
                ));
            }
            let volume = atoms(&trade["vol"])?;
            let loan = if parent.buy {
                atoms(&trade["cost"])?
            } else {
                volume.clone()
            };
            proved_volume = proved_volume.checked_add(&volume)?;
            proved_loan = proved_loan.checked_add(&loan)?;
            records.insert(id.into(),json!({"pair":parent.pair,"type":if parent.buy{"buy"}else{"sell"},"posstatus":"open","vol":trade["vol"],"vol_closed":"0","cost":trade["cost"],"nativeParentTrade":trade}));
        }
        if proved_volume != atoms(&parent_row["vol_exec"])?
            || proved_loan > parent.maximum_loan
            || proved_volume.is_zero()
        {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "The terminal entry's native quantity and full extension differ from its protected financial proof.",
            ));
        }
        let mut issued = Quantity::zero();
        let mut child_ids = Vec::new();
        let mut selected_parameters = Value::Null;
        for (id, current) in inventory
            .iter()
            .filter(|(id, r)| margin_child_belongs(id, r, &parent_id, &previous.binding))
        {
            let approved = previous.binding["conditionalChildRevisions"][id]["parameters"]
                .as_object()
                .map(|_| &previous.binding["conditionalChildRevisions"][id]["parameters"])
                .unwrap_or(template.conditional_parameters.as_ref().ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                        "The immutable closing template is unavailable.",
                    )
                })?);
            if current["descr"]["pair"] != template.pair
                || current["descr"]["type"] != if template.buy { "sell" } else { "buy" }
                || current["descr"]["ordertype"] != approved["ordertype"]
            {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                    "The issued closing order changed native market, direction or type.",
                ));
            }
            for field in ["price", "price2"] {
                if let Some(price) = approved.get(field)
                    && atoms(price)? != atoms(&current["descr"][field])?
                {
                    return Err(error(
                        "KRAKEN_MARGIN_CHILD_PRICE_MISMATCH",
                        "The issued child changed an approved trigger or execution limit.",
                    ));
                }
            }
            let retired =
                previous.binding["conditionalChildRevisions"][id]["retiredBySuccessor"].is_string();
            let quantity = atoms(&current[if retired { "vol_exec" } else { "vol" }])?;
            issued = issued.checked_add(&quantity)?;
            child_ids.push(id.clone());
            if id == &target {
                let mut p = approved.clone();
                p["pair"] = json!(template.pair);
                p["type"] = json!(if template.buy { "sell" } else { "buy" });
                p["volume"] = json!(quantity.to_decimal(36));
                p["leverage"] = json!(template.leverage.to_string());
                p["cl_ord_id"] = json!(
                    row["cl_ord_id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!(
                            "fc{}",
                            &hex::encode(Sha256::digest(target.as_bytes()))[..16]
                        ))
                );
                selected_parameters = p;
            }
        }
        if issued > proved_volume || !selected_parameters.is_object() {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_SCOPE_EXCEEDED",
                "Issued margin closing quantities exceed the independently proved parent exposure.",
            ));
        }
        let mut selected = template.clone();
        selected.conditional_parent = None;
        selected.conditional_parameters = None;
        selected.generated_positions.clear();
        selected.client_id = selected_parameters["cl_ord_id"].as_str().unwrap().into();
        let lineage_owner =
            previous.binding["conditionalChildRevisions"][&target]["targetOriginalId"]
                .as_str()
                .unwrap_or(&target)
                .to_owned();
        selected.order_ids = child_ids
            .iter()
            .filter(|id| {
                id == &&target
                    || previous.binding["conditionalChildRevisions"][*id]["targetOriginalId"]
                        .as_str()
                        == Some(&lineage_owner)
            })
            .cloned()
            .collect();
        selected.volume = selected
            .order_ids
            .iter()
            .filter(|id| *id != &target)
            .try_fold(atoms(&row["vol"])?, |q, id| {
                q.checked_add(&atoms(&inventory[id]["vol_exec"])?)
            })?;
        selected.prior =
            position_totals(&Value::Object(records.clone()), &parent.pair, parent.buy)?;
        selected.maximum_loan = maximum_close_loan(&selected.prior, &selected.volume, parent.buy)?;
        let at = gofin_domain::now();
        let loan_asset = if parent.buy {
            parent.quote.clone()
        } else {
            parent.base.clone()
        };
        selected.prior_anchor = anchor(
            account,
            &parent.pair,
            parent.buy,
            Amount {
                asset: parent.base.clone(),
                atoms: selected.prior.volume.clone(),
            },
            false,
            &selected.prior.records,
            &at,
        );
        selected.prior_debt_anchor = anchor(
            account,
            &parent.pair,
            parent.buy,
            Amount {
                asset: loan_asset,
                atoms: selected.prior.loan.clone(),
            },
            true,
            &selected.prior.records,
            &at,
        );
        let mut virtual_intent = previous.intent.clone();
        virtual_intent.expected.retain(|e| match e {
            EffectConstraint::MarginPosition {
                position,
                minimum_delta,
                ..
            } => position == &position_key(&parent.pair, parent.buy) && minimum_delta.is_negative(),
            EffectConstraint::Debt {
                debt, direction, ..
            } => debt == &debt_key(&parent.pair, parent.buy) && *direction == Direction::Decrease,
            EffectConstraint::Settlement { asset, .. } => asset == &parent.quote,
            _ => false,
        });
        let mut virtual_binding = previous.binding.clone();
        virtual_binding["margin"] = json!(MarginBinding {
            orders: vec![selected.clone()],
            controls: margin.controls.clone(),
            validation_only: false
        });
        virtual_binding["parameters"] = selected_parameters;
        let virtual_scope = AmendmentScope {
            intent: virtual_intent,
            binding: virtual_binding,
            valuation: previous.valuation.clone(),
        };
        let mut prepared = if endpoint == E::KrakenPrivatePostEditOrder {
            self.prepare_margin_legacy_edit(
                input,
                account,
                parameters,
                controls,
                Some(&virtual_scope),
            )
            .await?
        } else {
            self.prepare_margin_amendment(
                input,
                account,
                parameters,
                controls,
                Some(&virtual_scope),
            )
            .await?
        };
        let isolated = prepared
            .intent
            .expected
            .iter()
            .find_map(|e| match e {
                EffectConstraint::SubmissionAmendment { revision, .. } => {
                    Some((**revision).clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                    "The exact native closing revision is unavailable.",
                )
            })?;
        let revised: MarginBinding = serde_json::from_value(isolated.binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                    "The revised closing quantity is unavailable.",
                )
            })?;
        if issued
            .checked_sub(&selected.volume)?
            .checked_add(&revised.orders[0].volume)?
            > proved_volume
        {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_SCOPE_EXCEEDED",
                "The revised child and all sibling closing allocations exceed the independently proved parent exposure.",
            ));
        }
        let limit_field = if isolated.binding["parameters"]["ordertype"] == "limit" {
            "price"
        } else {
            "price2"
        };
        let limit = absolute_execution_price(&isolated.binding["parameters"][limit_field])?;
        let quote = product(&revised.orders[0].volume, &limit)?;
        let loss_cap = controls
            .maximum_loss
            .as_ref()
            .or(margin.controls.maximum_loss.as_ref())
            .map(|v| Quantity::from_decimal(v, 36))
            .transpose()?
            .ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                    "The issued close retains an explicit whole-owner maximumLoss.",
                )
            })?;
        let loss_share = loss_cap.mul_div_floor(
            &Quantity::from_u64(1),
            &Quantity::from_u64(
                margin
                    .orders
                    .iter()
                    .filter(|o| o.conditional_parent.is_some())
                    .count() as u64,
            ),
        )?;
        let loan_share =
            maximum_close_loan(&selected.prior, &revised.orders[0].volume, parent.buy)?;
        if parent.buy && loan_share > quote.checked_add(&loss_share)?
            || !parent.buy
                && quote
                    > minimum_entry_quote(&records, &revised.orders[0].volume)?
                        .checked_add(&loss_share)?
        {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_LOSS_CAP",
                "The revised child's exact native execution limit exceeds its share of the original whole-owner realized loss ceiling.",
            ));
        }
        let mut full = previous.intent.clone();
        full.maximum_costs = isolated.intent.maximum_costs.clone();
        for debit in &mut full.debits {
            if let Some(new) = isolated
                .intent
                .debits
                .iter()
                .find(|d| d.resource == debit.resource && d.amount.asset == debit.amount.asset)
            {
                debit.amount.atoms = new.amount.atoms.clone();
            }
        }
        for expected in &mut full.expected {
            if let EffectConstraint::Settlement {
                resource,
                asset,
                category,
                minimum_delta,
                maximum_delta,
            } = expected
                && let Some(EffectConstraint::Settlement{minimum_delta:new_min,maximum_delta:new_max,..})=isolated.intent.expected.iter().find(|e|matches!(e,EffectConstraint::Settlement{resource:r,asset:a,category:c,minimum_delta:m,..}if r==resource&&a==asset&&c==category&&m.is_negative()==minimum_delta.is_negative()))
            { *minimum_delta=new_min.clone();*maximum_delta=new_max.clone(); }
        }
        let mut own = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for new in &full.debits {
            let old = previous
                .intent
                .debits
                .iter()
                .find(|d| d.resource == new.resource && d.amount.asset == new.amount.asset)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                        "Closing source identity changed.",
                    )
                })?;
            if new.amount.atoms > old.amount.atoms {
                let mut delta = new.clone();
                delta.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                own.debits.push(delta);
            }
        }
        for new in &full.maximum_costs {
            let old = previous
                .intent
                .maximum_costs
                .iter()
                .find(|d| {
                    d.location == new.location
                        && d.amount.asset == new.amount.asset
                        && d.treatment == new.treatment
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                        "Closing cost identity changed.",
                    )
                })?;
            if new.amount.atoms > old.amount.atoms {
                let mut delta = new.clone();
                delta.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                own.maximum_costs.push(delta);
            }
        }
        margin.orders[index].order_ids = child_ids;
        if !controls.maximum_fees.is_empty() {
            margin.controls.maximum_fees = controls.maximum_fees.clone();
        }
        if controls.maximum_loss.is_some() {
            margin.controls.maximum_loss = controls.maximum_loss.clone();
        }
        if controls.maximum_profit.is_some() {
            margin.controls.maximum_profit = controls.maximum_profit.clone();
        }
        let mut binding = previous.binding.clone();
        binding["margin"] = json!(margin);
        let mut native_child_parameters = isolated.binding["parameters"].clone();
        if endpoint == E::KrakenPrivatePostEditOrder {
            native_child_parameters["volume"] = prepared.binding["parameters"]["volume"].clone();
        }
        let originally_issued = previous.binding["conditionalChildRevisions"][&target]
            .get("originalIssuedVolume")
            .cloned()
            .unwrap_or(row["vol"].clone());
        binding["conditionalChildRevisions"][&target] = json!({"parentOrderId":parent_id,"childIndex":index,"parameters":native_child_parameters,"originalIssuedVolume":originally_issued,"approvedCumulativeVolume":revised.orders[0].volume});
        binding["conditionalChildRevisions"][&target]["targetOriginalId"] = json!(lineage_owner);
        if endpoint == E::KrakenPrivatePostEditOrder {
            binding["conditionalChildRevisions"][&target]["legacyRevision"] = json!(true);
            binding["conditionalChildRevisions"][&target]["predecessorParameters"] =
                virtual_scope.binding["parameters"].clone();
            binding["conditionalChildRevisions"][&target]["nativeSuccessorQuantity"] =
                prepared.binding["parameters"]["volume"].clone();
        }
        check_aggregate_child_loss(&binding, &parent, &template, &parent_id, &loss_share)?;
        let full_margin: MarginBinding = serde_json::from_value(binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                    "Full margin scope unavailable.",
                )
            })?;
        let mut scopes = Vec::new();
        for order in &full_margin.orders {
            let native = if order.client_id == template.client_id {
                &isolated.binding["parameters"]
            } else {
                order.conditional_parameters.as_ref().unwrap_or_else(|| {
                    if binding["parameters"]["orders"].is_array() {
                        binding["parameters"]["orders"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|p| p["cl_ord_id"] == order.client_id)
                            .unwrap_or(&binding["parameters"])
                    } else {
                        &binding["parameters"]
                    }
                })
            };
            scopes.push(
                self.margin_fee_scope(
                    crate::margin_fee_admission::MarginFeeScope {
                        pair: order.pair.clone(),
                        base: order.base.clone(),
                        quote: order.quote.clone(),
                        volume: order.volume.clone(),
                        quote_notional: Quantity::zero(),
                        settling: order.settling,
                    },
                    native,
                )
                .await?,
            );
        }
        let caps = full
            .maximum_costs
            .iter()
            .filter(|c| c.treatment == CostTreatment::AdditionalDebit)
            .map(|c| {
                (
                    c.amount.asset.reference.clone(),
                    c.amount.atoms.to_decimal(36),
                )
            })
            .collect();
        let admission = self
            .verify_margin_fee_admission(account, &scopes, &caps)
            .await?;
        binding["marginFeeAdmission"] = admission.clone();
        prepared.binding["marginFeeAdmission"] = admission;

        binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::value_bounds::KEY);
        let gross = margin_gross(
            &serde_json::from_value::<MarginBinding>(binding["margin"].clone())
                .map_err(|_| {
                    error(
                        "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                        "Full margin scope unavailable.",
                    )
                })?
                .orders,
            account,
        );
        let mut valuation = self
            .financial_valuation(&gross, &full.maximum_costs)
            .await?;
        if let Some(old) = &previous.valuation {
            valuation.movement_usd_micros = valuation
                .movement_usd_micros
                .max(old.movement_usd_micros.clone());
        }
        own.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: controls
                .operation_id
                .clone()
                .ok_or_else(|| error("INVALID_INPUT", "The original operationId is required."))?,
            submission: submission.clone(),
            revision: Box::new(AmendmentScope {
                intent: full,
                binding,
                valuation: Some(valuation),
            }),
        });
        prepared.intent = own;
        prepared.valuation = Some(
            self.financial_valuation(&prepared.intent.debits, &prepared.intent.maximum_costs)
                .await?,
        );
        prepared.binding["amendmentPreviousScope"] = json!(previous);
        prepared.binding["conditionalOriginalEvidence"] = json!(evidence);
        prepared.binding["conditionalMarginChildRevision"] = json!(true);
        prepared.binding["amendment"]["conditionalChildIndex"] = json!(index);
        prepared.binding["amendment"]["batchOrderIndex"] = json!(index);
        prepared
            .binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::value_bounds::KEY);
        prepared.summary = format!(
            "Revise issued margin child {target} on {}: close {} {}, execution limit {} {}, trigger {}. Parent {parent_id} retains its full {} {} loan and original fill accounting.",
            account.name,
            revised.orders[0].volume.to_decimal(36),
            parent.base.reference,
            limit.to_decimal(36),
            parent.quote.reference,
            isolated.binding["parameters"]["price"]
                .as_str()
                .filter(|_| isolated.binding["parameters"]["ordertype"] != "limit")
                .unwrap_or("unchanged"),
            proved_loan.to_decimal(36),
            if parent.buy {
                &parent.quote.reference
            } else {
                &parent.base.reference
            }
        );
        prepared.checks.push(gofin_domain::Check{name:"issued margin child, parent trades and full extension".into(),status:gofin_domain::CheckStatus::Passed,detail:format!("Exact native child {target} links to parent {parent_id}; {} {} parent exposure and {} {} loan are independently proved by immutable native trades and custody ledgers. Other children and the parent template remain unchanged.",proved_volume.to_decimal(36),parent.base.reference,proved_loan.to_decimal(36),if parent.buy{&parent.quote.reference}else{&parent.base.reference})});
        if prepared
            .valuation
            .as_ref()
            .is_some_and(|v| !v.prices.is_empty())
        {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
}
fn margin_gross(orders: &[MarginOrder], account: &KrakenAccount) -> Vec<ResourceAmount> {
    orders
        .iter()
        .map(|o| ResourceAmount {
            resource: account.resource.clone(),
            amount: Amount {
                asset: if o.buy {
                    o.quote.clone()
                } else {
                    o.base.clone()
                },
                atoms: o.maximum_loan.clone(),
            },
        })
        .collect()
}

pub(super) fn margin_child_belongs(id: &str, row: &Value, parent: &str, binding: &Value) -> bool {
    row["refid"] == parent
        || (binding["conditionalChildRevisions"][id]["parentOrderId"] == parent
            && binding["conditionalChildRevisions"][id]["predecessorOrderId"]
                .as_str()
                .is_some_and(|p| row["refid"] == p))
}
pub(super) fn margin_child_goal(
    binding: &Value,
    parent: &str,
    filled: &Quantity,
) -> Result<Quantity> {
    let Some(overrides) = binding["conditionalChildRevisions"].as_object() else {
        return Ok(filled.clone());
    };
    let mut goal = filled.clone();
    for (root, scope) in overrides
        .iter()
        .filter(|(_, s)| s["parentOrderId"] == parent && s.get("predecessorOrderId").is_none())
    {
        let mut latest = scope;
        let mut seen = BTreeSet::from([root.as_str()]);
        while let Some(next) = latest["retiredBySuccessor"].as_str() {
            if !seen.insert(next) {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_LINEAGE_INVALID",
                    "A protected native closing lineage repeats an order identity.",
                ));
            }
            latest = overrides.get(next).ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_LINEAGE_INVALID",
                    "Protected closing successor scope missing.",
                )
            })?;
        }
        if let (Some(old), Some(new)) = (
            scope.get("originalIssuedVolume"),
            latest.get("approvedCumulativeVolume"),
        ) {
            let new: Quantity = serde_json::from_value(new.clone()).map_err(|_| {
                error(
                    "KRAKEN_MARGIN_CHILD_REVISION_INVALID",
                    "The approved closing quantity is invalid.",
                )
            })?;
            goal = goal.checked_sub(&atoms(old)?)?.checked_add(&new)?;
        }
    }
    Ok(goal)
}

/// A native closing buy may repay any generated short lot. Do not infer FIFO or
/// average the entry proceeds: the lowest-priced eligible lots bound worst loss.
fn minimum_entry_quote(
    records: &serde_json::Map<String, Value>,
    volume: &Quantity,
) -> Result<Quantity> {
    let mut lots = records
        .values()
        .map(|r| Ok((atoms(&r["cost"])?, positive(&r["vol"])?)))
        .collect::<Result<Vec<_>>>()?;
    lots.sort_by(|a, b| (a.0.integer() * b.1.integer()).cmp(&(b.0.integer() * a.1.integer())));
    let mut remaining = volume.clone();
    let mut minimum = Quantity::zero();
    for (cost, size) in lots {
        let taken = remaining.clone().min(size.clone());
        minimum = minimum.checked_add(&cost.mul_div_floor(&taken, &size)?)?;
        remaining = remaining.checked_sub(&taken)?;
    }
    if !remaining.is_zero() {
        return Err(error(
            "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED",
            "Revised native short close exceeds the independently proved parent lots.",
        ));
    }
    Ok(minimum)
}

fn execution_limit(parameters: &Value) -> Result<Quantity> {
    absolute_execution_price(
        &parameters[if parameters["ordertype"] == "limit" {
            "price"
        } else {
            "price2"
        }],
    )
}
/// Reserve the original whole template's adverse execution and every additional
/// adverse native revision. Favorable price changes never finance another child.
fn check_aggregate_child_loss(
    binding: &Value,
    parent: &MarginOrder,
    template: &MarginOrder,
    parent_id: &str,
    cap: &Quantity,
) -> Result<()> {
    let native = template.conditional_parameters.as_ref().ok_or_else(|| {
        error(
            "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
            "Original child limit unavailable.",
        )
    })?;
    let original_limit = execution_limit(native)?;
    let original_quote = product(&template.volume, &original_limit)?;
    let original_entry = if parent.buy {
        parent.maximum_loan.clone()
    } else {
        let entry = if let Some(rows) = binding["parameters"]["orders"].as_array() {
            rows.iter()
                .find(|p| p["cl_ord_id"] == parent.client_id)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                        "Exact original entry price bound missing.",
                    )
                })?
        } else {
            &binding["parameters"]
        };
        product(&template.volume, &execution_limit(entry)?)?
    };
    let mut loss = if parent.buy {
        original_entry
            .clone()
            .max(original_quote.clone())
            .checked_sub(&original_quote)?
    } else {
        original_quote
            .clone()
            .max(original_entry.clone())
            .checked_sub(&original_entry)?
    };
    if let Some(overrides) = binding["conditionalChildRevisions"].as_object() {
        for (_, scope) in overrides.iter().filter(|(_, scope)| {
            scope["parentOrderId"] == parent_id && scope.get("retiredBySuccessor").is_none()
        }) {
            let limit = execution_limit(&scope["parameters"])?;
            let adverse = if parent.buy {
                original_limit
                    .clone()
                    .max(limit.clone())
                    .checked_sub(&limit)?
            } else {
                limit
                    .clone()
                    .max(original_limit.clone())
                    .checked_sub(&original_limit)?
            };
            let volume = atoms(&scope["parameters"]["volume"])?;
            loss = loss.checked_add(&product(&volume, &adverse)?)?;
        }
    }
    if loss > *cap {
        return Err(error(
            "KRAKEN_MARGIN_CHILD_LOSS_CAP",
            "All issued closing revisions and the unchanged native template exceed their shared whole-owner realized-loss allowance. Favorable revisions cannot offset another child's adverse allowance.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod loss_budget_tests {
    use super::*;
    #[test]
    fn native_siblings_share_one_loss_ceiling_in_both_directions_without_favorable_offsets() {
        let a = KrakenAccount {
            name: "synthetic".into(),
            resource: gofin_domain::ResourceId("synthetic".into()),
            credential_ref: "synthetic-only".into(),
            physical_identity: "synthetic-only".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: crate::KrakenProduct::Spot,
        };
        let base = AssetId {
            network: "kraken".into(),
            reference: "XXBT".into(),
            decimals: 36,
        };
        let quote = AssetId {
            network: "kraken".into(),
            reference: "ZUSD".into(),
            decimals: 36,
        };
        for buy in [true, false] {
            let prior = MarginTotals::default();
            let at = gofin_domain::now();
            let parent = MarginOrder {
                client_id: "entry".into(),
                order_ids: vec!["PARENT".into()],
                conditional_parent: None,
                conditional_parameters: None,
                generated_positions: BTreeMap::new(),
                pair: "XXBTZUSD".into(),
                base: base.clone(),
                quote: quote.clone(),
                buy,
                volume: Quantity::from_decimal("0.01", 36).unwrap(),
                maximum_loan: Quantity::from_decimal(if buy { "200" } else { "0.01" }, 36).unwrap(),
                leverage: 2,
                collateral_required: Quantity::zero(),
                user_reference: None,
                closing: false,
                settling: false,
                prior: prior.clone(),
                prior_anchor: anchor(
                    &a,
                    "XXBTZUSD",
                    buy,
                    Amount {
                        asset: base.clone(),
                        atoms: Quantity::zero(),
                    },
                    false,
                    &prior.records,
                    &at,
                ),
                prior_debt_anchor: anchor(
                    &a,
                    "XXBTZUSD",
                    buy,
                    Amount {
                        asset: if buy { quote.clone() } else { base.clone() },
                        atoms: Quantity::zero(),
                    },
                    true,
                    &prior.records,
                    &at,
                ),
            };
            let mut child = parent.clone();
            child.conditional_parent = Some("entry".into());
            child.conditional_parameters =
                Some(json!({"ordertype":"limit","price":if buy{"19000"}else{"21000"}}));
            let mut binding = json!({"parameters":{"ordertype":"limit","price":"20000"},"conditionalChildRevisions":{"A":{"parentOrderId":"PARENT","parameters":{"ordertype":"limit","price":if buy{"17000"}else{"23000"},"volume":"0.005"}}}});
            let cap = Quantity::from_decimal("20", 36).unwrap();
            assert!(check_aggregate_child_loss(&binding, &parent, &child, "PARENT", &cap).is_ok());
            binding["conditionalChildRevisions"]["B"] =
                binding["conditionalChildRevisions"]["A"].clone();
            assert_eq!(
                check_aggregate_child_loss(&binding, &parent, &child, "PARENT", &cap)
                    .unwrap_err()
                    .code,
                "KRAKEN_MARGIN_CHILD_LOSS_CAP"
            );
            binding["conditionalChildRevisions"]["A"]["parameters"]["price"] =
                json!(if buy { "16000" } else { "24000" });
            binding["conditionalChildRevisions"]["B"]["parameters"]["price"] = json!("20000");
            assert_eq!(
                check_aggregate_child_loss(&binding, &parent, &child, "PARENT", &cap)
                    .unwrap_err()
                    .code,
                "KRAKEN_MARGIN_CHILD_LOSS_CAP"
            );
        }
    }
}

#[cfg(test)]
mod price_bounds {
    use super::*;
    #[test]
    fn native_short_close_uses_lowest_entry_proceeds_without_average_or_fifo() {
        let lots = json!({"LOW":{"vol":"1","cost":"100"},"HIGH":{"vol":"1","cost":"400"}});
        assert_eq!(
            minimum_entry_quote(
                lots.as_object().unwrap(),
                &Quantity::from_decimal("1", 36).unwrap()
            )
            .unwrap(),
            Quantity::from_decimal("100", 36).unwrap()
        );
    }
}
