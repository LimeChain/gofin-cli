//! A native batch member changes under the original whole-batch fill owner.
//! All unselected orders, native baselines and global fee/PnL ceilings survive.
use super::*;
use fin_domain::amendments::AmendmentScope;

fn scope_intent(
    account: &KrakenAccount,
    orders: &[MarginOrder],
    controls: &TradingControls,
    costs: &[CostBound],
) -> Result<FinancialIntent> {
    let mut intent = FinancialIntent {
        locks: vec![account.resource.clone()],
        maximum_costs: costs.to_vec(),
        ..Default::default()
    };
    let mut pnl = BTreeSet::new();
    for order in grouped_orders(orders)? {
        let loan_asset = if order.buy { &order.quote } else { &order.base };
        intent.expected.push(EffectConstraint::MarginPosition {
            resource: account.resource.clone(),
            position: position_key(&order.pair, order.buy),
            asset: order.base.clone(),
            position_kind: kind(order.buy),
            minimum_delta: if order.closing {
                SignedQuantity::negative(&order.volume)
            } else {
                SignedQuantity::positive(&Quantity::zero())
            },
            maximum_delta: if order.closing {
                SignedQuantity::positive(&Quantity::zero())
            } else {
                SignedQuantity::positive(&order.volume)
            },
        });
        intent.expected.push(EffectConstraint::Debt {
            resource: account.resource.clone(),
            debt: debt_key(&order.pair, order.buy),
            asset: loan_asset.clone(),
            direction: if order.closing {
                Direction::Decrease
            } else {
                Direction::Increase
            },
            maximum_change: order.maximum_loan.clone(),
        });
        if order.settling {
            intent.debits.push(ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: loan_asset.clone(),
                    atoms: order.maximum_loan.clone(),
                },
            });
            intent.expected.push(EffectConstraint::Debit {
                resource: account.resource.clone(),
                asset: loan_asset.clone(),
                maximum_debit: order.maximum_loan.clone(),
            });
            intent.expected.push(EffectConstraint::Credit {
                to: account.resource.clone(),
                asset: if order.buy {
                    order.base.clone()
                } else {
                    order.quote.clone()
                },
                minimum_credit: Quantity::zero(),
            });
        } else if order.closing && pnl.insert(order.quote.clone()) {
            let loss = Quantity::from_decimal(
                controls.maximum_loss.as_deref().ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                        "Retain the whole original batch maximumLoss.",
                    )
                })?,
                36,
            )?;
            let profit = Quantity::from_decimal(
                controls.maximum_profit.as_deref().ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                        "Retain the whole original batch maximumProfit.",
                    )
                })?,
                36,
            )?;
            for (minimum_delta, maximum_delta) in [
                (
                    SignedQuantity::negative(&loss),
                    SignedQuantity::positive(&Quantity::zero()),
                ),
                (
                    SignedQuantity::positive(&Quantity::zero()),
                    SignedQuantity::positive(&profit),
                ),
            ] {
                intent.expected.push(EffectConstraint::Settlement {
                    resource: account.resource.clone(),
                    asset: order.quote.clone(),
                    category: SettlementCategory::RealizedPnl,
                    minimum_delta,
                    maximum_delta,
                });
            }
            intent.debits.push(ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: order.quote.clone(),
                    atoms: loss,
                },
            });
        }
    }
    Ok(intent)
}

fn totals(
    rows: &[ResourceAmount],
) -> Result<BTreeMap<(fin_domain::ResourceId, AssetId), Quantity>> {
    let mut totals = BTreeMap::new();
    for row in rows {
        let total = totals
            .entry((row.resource.clone(), row.amount.asset.clone()))
            .or_insert_with(Quantity::zero);
        *total = total.checked_add(&row.amount.atoms)?;
    }
    Ok(totals)
}
impl KrakenProfile {
    pub(crate) async fn prepare_margin_batch_revision(
        &self,
        endpoint: E,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: &AmendmentScope,
    ) -> Result<PreparedOperation> {
        let mut margin: MarginBinding = serde_json::from_value(previous.binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Original margin batch scope is unavailable.",
                )
            })?;
        if margin.validation_only
            || !(2..=15).contains(&margin.orders.len())
            || previous.binding["endpoint"] != E::KrakenPrivatePostAddOrderBatch.method()
            || margin
                .orders
                .iter()
                .any(|order| order.conditional_parent.is_some())
            || !previous.binding["parameters"]["orders"].is_array()
        {
            return Err(error(
                "KRAKEN_MARGIN_BATCH_REVISION_SCOPE_INVALID",
                "Select an original native margin batch without generated conditional children; linked conditional revisions require separate child evidence.",
            ));
        }
        let open = self
            .json(
                E::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let rows = result(&open)["open"].as_object().ok_or_else(|| {
            error(
                "KRAKEN_ORDER_PROOF_PENDING",
                "Native original batch order identities are unavailable.",
            )
        })?;
        let candidates = margin
            .orders
            .iter()
            .enumerate()
            .flat_map(|(index, order)| {
                rows.iter()
                    .filter_map(|(id, row)| {
                        let owned = if !order.order_ids.is_empty() {
                            order.order_ids.contains(id)
                        } else if let Some(reference) = &order.user_reference {
                            row["userref"] == *reference
                        } else {
                            row["cl_ord_id"] == order.client_id
                        };
                        let selected = parameters["txid"]
                            .as_str()
                            .is_some_and(|target| target == id)
                            || parameters["cl_ord_id"]
                                .as_str()
                                .is_some_and(|target| target == order.client_id)
                            || parameters["txid"]
                                .as_i64()
                                .is_some_and(|target| row["userref"].as_i64() == Some(target));
                        (owned && selected).then_some(index)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Select exactly one proven original margin batch member; shared references cannot identify multiple children.",
            ));
        }
        let selected = candidates[0];
        let old = margin.orders[selected].clone();
        let mut cumulative_controls = controls.clone();
        let mut scoped_input = input.clone();
        if cumulative_controls.maximum_loss.is_none() {
            cumulative_controls.maximum_loss = margin.controls.maximum_loss.clone();
        }
        if cumulative_controls.maximum_profit.is_none() {
            cumulative_controls.maximum_profit = margin.controls.maximum_profit.clone();
        }
        let full_controls = cumulative_controls.clone();
        if old.closing {
            cumulative_controls.maximum_spend = None;
            scoped_input.as_object_mut().unwrap().remove("maximumSpend");
        } else {
            cumulative_controls.maximum_loss = None;
            cumulative_controls.maximum_profit = None;
            scoped_input.as_object_mut().unwrap().remove("maximumLoss");
            scoped_input
                .as_object_mut()
                .unwrap()
                .remove("maximumProfit");
        }
        let mut virtual_binding = previous.binding.clone();
        let mut singleton = margin.clone();
        singleton.orders = vec![old.clone()];
        virtual_binding["margin"] = json!(singleton);
        let mut native = previous.binding["parameters"]["orders"][selected].clone();
        native["pair"] = json!(old.pair);
        virtual_binding["parameters"] = native;
        let virtual_scope = AmendmentScope {
            binding: virtual_binding,
            valuation: previous.valuation.clone(),
            intent: scope_intent(
                account,
                std::slice::from_ref(&old),
                &margin.controls,
                &previous
                    .intent
                    .maximum_costs
                    .iter()
                    .filter(|cost| old.closing || cost.treatment != CostTreatment::IncludedInDebit)
                    .cloned()
                    .collect::<Vec<_>>(),
            )?,
        };
        let mut prepared = match endpoint {
            E::KrakenPrivatePostAmendOrder => {
                Box::pin(self.prepare_margin_amendment(
                    &scoped_input,
                    account,
                    parameters,
                    &cumulative_controls,
                    Some(&virtual_scope),
                ))
                .await?
            }
            E::KrakenPrivatePostEditOrder => {
                Box::pin(self.prepare_margin_legacy_edit(
                    &scoped_input,
                    account,
                    parameters,
                    &cumulative_controls,
                    Some(&virtual_scope),
                ))
                .await?
            }
            _ => {
                return Err(error(
                    "KRAKEN_MARGIN_BATCH_REVISION_SCOPE_INVALID",
                    "Use the documented atomic amendment or legacy edit operation.",
                ));
            }
        };
        let (operation_id, submission, isolated) = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment {
                    operation_id,
                    submission,
                    revision,
                } => Some((
                    operation_id.clone(),
                    submission.clone(),
                    (**revision).clone(),
                )),
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Selected member revision omitted its original authority owner.",
                )
            })?;
        let revised: MarginBinding = serde_json::from_value(isolated.binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Revised member margin scope is unavailable.",
                )
            })?;
        margin.orders[selected] = revised.orders[0].clone();
        margin.controls = full_controls;
        let mut costs = previous
            .intent
            .maximum_costs
            .iter()
            .filter(|cost| cost.treatment == CostTreatment::IncludedInDebit)
            .cloned()
            .collect::<Vec<_>>();
        costs.extend(
            isolated
                .intent
                .maximum_costs
                .iter()
                .filter(|cost| cost.treatment != CostTreatment::IncludedInDebit)
                .cloned(),
        );
        for cost in &mut costs {
            if cost.treatment == CostTreatment::IncludedInDebit
                && margin.orders.iter().any(|order| {
                    order.closing && !order.settling && order.quote == cost.amount.asset
                })
                && let Some(loss) = &margin.controls.maximum_loss
            {
                cost.amount.atoms = Quantity::from_decimal(loss, 36)?;
            }
        }
        let mut full = scope_intent(account, &margin.orders, &margin.controls, &costs)?;
        full.locks = previous.intent.locks.clone();
        super::check_principal_cap(endpoint, &margin.orders, controls.maximum_spend.as_deref())?;
        let before = totals(&previous.intent.debits)?;
        let after = totals(&full.debits)?;
        let mut own = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for ((resource, asset), maximum) in after {
            let old = before
                .get(&(resource.clone(), asset.clone()))
                .cloned()
                .unwrap_or_default();
            if maximum > old {
                own.debits.push(ResourceAmount {
                    resource,
                    amount: Amount {
                        asset,
                        atoms: maximum.checked_sub(&old)?,
                    },
                });
            }
        }
        for cost in &full.maximum_costs {
            let old = previous
                .intent
                .maximum_costs
                .iter()
                .find(|old| {
                    old.location == cost.location
                        && old.amount.asset == cost.amount.asset
                        && old.treatment == cost.treatment
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                        "The original global fee/PnL asset identities must survive.",
                    )
                })?;
            if cost.amount.atoms > old.amount.atoms {
                let mut delta = cost.clone();
                delta.amount.atoms = cost.amount.atoms.checked_sub(&old.amount.atoms)?;
                own.maximum_costs.push(delta);
            }
        }
        let principal = margin
            .orders
            .iter()
            .map(|order| ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: if order.buy {
                        order.quote.clone()
                    } else {
                        order.base.clone()
                    },
                    atoms: order.maximum_loan.clone(),
                },
            })
            .collect::<Vec<_>>();
        let new_order = &margin.orders[selected];
        let increment = if new_order.maximum_loan > old.maximum_loan {
            new_order.maximum_loan.checked_sub(&old.maximum_loan)?
        } else {
            Quantity::zero()
        };
        let own_principal = [ResourceAmount {
            resource: account.resource.clone(),
            amount: Amount {
                asset: if old.buy {
                    old.quote.clone()
                } else {
                    old.base.clone()
                },
                atoms: increment,
            },
        }];
        let mut full_binding = previous.binding.clone();
        full_binding["margin"] = json!(margin);
        let mut revised_native = isolated.binding["parameters"].clone();
        revised_native
            .as_object_mut()
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Revised native member request is unavailable.",
                )
            })?
            .remove("pair");
        full_binding["parameters"]["orders"][selected] = revised_native;
        let mut fee_scopes = Vec::new();
        for (index, order) in margin.orders.iter().enumerate() {
            fee_scopes.push(
                self.margin_fee_scope(
                    crate::margin_fee_admission::MarginFeeScope {
                        pair: order.pair.clone(),
                        base: order.base.clone(),
                        quote: order.quote.clone(),
                        volume: order.volume.clone(),
                        quote_notional: Quantity::zero(),
                        settling: order.settling,
                    },
                    &full_binding["parameters"]["orders"][index],
                )
                .await?,
            );
        }
        let fee_caps = full
            .maximum_costs
            .iter()
            .filter(|cost| cost.treatment == CostTreatment::AdditionalDebit)
            .map(|cost| {
                (
                    cost.amount.asset.reference.clone(),
                    cost.amount.atoms.to_decimal(36),
                )
            })
            .collect();
        let fee_admission = self
            .verify_margin_fee_admission(account, &fee_scopes, &fee_caps)
            .await?;
        full_binding["marginFeeAdmission"] = fee_admission.clone();
        full_binding
            .as_object_mut()
            .unwrap()
            .remove(fin_domain::value_bounds::KEY);
        let revision = AmendmentScope {
            binding: full_binding,
            valuation: Some(
                self.financial_valuation(&principal, &full.maximum_costs)
                    .await?,
            ),
            intent: full,
        };
        own.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id,
            submission,
            revision: Box::new(revision),
        });
        prepared.valuation = Some(
            self.financial_valuation(&own_principal, &own.maximum_costs)
                .await?,
        );
        prepared.intent = own;
        prepared.input = input.clone();
        warnings_from_fee_evidence(&mut prepared, &fee_admission);
        prepared.binding["amendmentPreviousScope"] = json!(previous);
        prepared.binding["amendment"]["batchOrderIndex"] = json!(selected);
        prepared.summary = format!(
            "Revise original Kraken margin batch member {}; retain all unselected orders, full loans and original fill ownership.",
            selected + 1
        );
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use async_trait::async_trait;
    use fin_domain::{Adapter, ResourceId, amendments::AmendmentContext, now};
    use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
    use fin_pricing::{UsdPriceSource, UsdQuote};
    use std::sync::{Arc, Mutex};
    struct Prices;
    #[async_trait]
    impl UsdPriceSource for Prices {
        async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
            Ok(UsdQuote {
                usd_per_unit: if asset.reference == "XXBT" {
                    "20000"
                } else {
                    "1"
                }
                .into(),
                updated_at: now(),
                observed_at: now(),
                source: "synthetic exact native unit".into(),
            })
        }
    }
    #[derive(Default)]
    struct Venue {
        calls: Mutex<Vec<(E, Value)>>,
        original: Mutex<Value>,
        amended: Mutex<Option<Value>>,
    }
    impl Venue {
        fn order(&self, index: usize) -> Value {
            let original = self.original.lock().unwrap();
            let child = &original["orders"][index];
            let amended = self.amended.lock().unwrap();
            let current = if index == 0 { amended.as_ref() } else { None };
            json!({"cl_ord_id":child["cl_ord_id"],"descr":{"pair":"XXBTZUSD","type":child["type"],"ordertype":"limit","price":current.map(|v|v["limit_price"].clone()).unwrap_or_else(||json!("20000"))},"status":"open","vol":current.map(|v|v["order_qty"].clone()).unwrap_or_else(||json!("0.02")),"vol_exec":"0","cost":"0","fee":"0","trades":[]})
        }
    }
    #[async_trait]
    impl KrakenTransport for Venue {
        async fn request(
            &self,
            endpoint: E,
            _: Option<&KrakenAccount>,
            parameters: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            if endpoint.mutating() {
                assert!(guard.as_ref().is_some_and(|guard| guard()));
            } else {
                assert!(guard.is_none());
            }
            self.calls
                .lock()
                .unwrap()
                .push((endpoint, parameters.clone()));
            Ok(KrakenResponse::Json(match endpoint {
                E::KrakenPrivatePostGetApiKeyInfo => {
                    json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0"}})
                }
                E::KrakenPublicGetAssets => {
                    json!({"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
                }
                E::KrakenPublicGetAssetPairs => {
                    json!({"result":{"XXBTZUSD":{"base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"leverage_buy":[2,3],"leverage_sell":[2,3]}}})
                }
                E::KrakenPrivatePostTradeBalance => json!({"result":{"mf":"10000"}}),
                E::KrakenPrivatePostTradeVolume => {
                    json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.2"}},"fees_maker":{"XXBTZUSD":{"fee":"0.1"}}}})
                }
                E::KrakenPublicGetTicker => {
                    json!({"result":{"XXBTZUSD":{"a":["20000","1","1"],"b":["20000","1","1"]}}})
                }
                E::KrakenPrivatePostOpenPositions => json!({"result":{}}),
                E::KrakenPrivatePostOpenOrders => {
                    json!({"result":{"open":{"O1":self.order(0),"O2":self.order(1)}}})
                }
                E::KrakenPrivatePostQueryOrders => {
                    let mut rows = serde_json::Map::new();
                    for id in parameters["txid"].as_str().unwrap().split(',') {
                        rows.insert(id.into(), self.order(if id == "O1" { 0 } else { 1 }));
                    }
                    json!({"result":rows})
                }
                E::KrakenPrivatePostAddOrderBatch => {
                    *self.original.lock().unwrap() = parameters;
                    json!({"result":{"orders":[{"txid":"O1"},{"txid":"O2"}]}})
                }
                E::KrakenPrivatePostAmendOrder => {
                    assert_eq!(parameters["txid"], "O1");
                    *self.amended.lock().unwrap() = Some(parameters);
                    json!({"result":{"amend_id":"SYNTHETIC-MARGIN-BATCH-AUDIT"}})
                }
                E::KrakenPrivatePostOrderAmends => {
                    let mut row = self.amended.lock().unwrap().clone().unwrap();
                    row["amend_id"] = json!("SYNTHETIC-MARGIN-BATCH-AUDIT");
                    json!({"result":{"amends":[row]}})
                }
                _ => panic!("Unexpected synthetic margin batch preview endpoint {endpoint:?}"),
            }))
        }
    }
    async fn fixture() -> (
        ExchangeAdapter<KrakenProfile>,
        Arc<Venue>,
        PreparedOperation,
    ) {
        let venue = Arc::new(Venue::default());
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic-margin-batch-edit".into()),
            credential_ref: "synthetic-key-only".into(),
            physical_identity: "SYNTHETIC-NOT-LIVE".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        let adapter = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], venue.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        );
        let mut original = adapter.prepare(E::KrakenPrivatePostAddOrderBatch.operation(), &json!({"account":"spot","requestId":"original-margin-batch","pair":"XXBTZUSD","orders":[{"type":"buy","ordertype":"limit","price":"20000","volume":"0.02","leverage":"2"},{"type":"buy","ordertype":"limit","price":"20000","volume":"0.02","leverage":"2"}],"maximumFees":{"XXBT":"0.0001","ZUSD":"2"}})).await.unwrap();
        *venue.original.lock().unwrap() = original.binding["parameters"].clone();
        original.binding["margin"]["orders"][0]["orderIds"] = json!(["O1"]);
        original.binding["margin"]["orders"][1]["orderIds"] = json!(["O2"]);
        (adapter, venue, original)
    }
    fn mutation(legacy: bool, quantity: &str, maximum_spend: Option<&str>) -> Value {
        let submission = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "SYNTHETIC-IMMUTABLE-ORIGINAL".into(),
            data: json!({}),
        };
        let mut input = json!({"account":"spot","requestId":"margin-batch-member-revision","operationId":"SYNTHETIC-ORIGINAL","originalSubmission":submission,"txid":"O1","maximumFees":{"XXBT":"0.00015","ZUSD":"3"}});
        if legacy {
            input["pair"] = json!("XXBTZUSD");
            input["volume"] = json!(quantity);
            input["price"] = json!("21000");
        } else {
            input["order_qty"] = json!(quantity);
            input["limit_price"] = json!("21000");
        }
        if let Some(cap) = maximum_spend {
            input["maximumSpend"] = json!(cap);
        }
        input
    }
    #[tokio::test]
    async fn native_margin_batch_member_revision_preserves_other_member_baselines_and_global_caps()
    {
        for legacy in [false, true] {
            let (adapter, venue, original) = fixture().await;
            let context = AmendmentContext {
                original: Some(original.clone()),
                ..Default::default()
            };
            let operation = if legacy {
                E::KrakenPrivatePostEditOrder
            } else {
                E::KrakenPrivatePostAmendOrder
            };
            let prepared = adapter
                .prepare_with_linked_context(
                    operation.operation(),
                    &mutation(legacy, "0.03", Some("1030")),
                    &Default::default(),
                    &context,
                )
                .await
                .unwrap();
            let revision = prepared
                .intent
                .expected
                .iter()
                .find_map(|effect| match effect {
                    EffectConstraint::SubmissionAmendment { revision, .. } => Some(revision),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                revision.binding["margin"]["orders"][1],
                original.binding["margin"]["orders"][1]
            );
            assert_eq!(
                revision.binding["parameters"]["orders"][1],
                original.binding["parameters"]["orders"][1]
            );
            assert_eq!(
                revision.binding[fin_domain::BASELINE_ANCHORS_KEY],
                original.binding[fin_domain::BASELINE_ANCHORS_KEY]
            );
            assert_eq!(
                revision
                    .intent
                    .maximum_costs
                    .iter()
                    .find(|cost| cost.amount.asset.reference == "ZUSD")
                    .unwrap()
                    .amount
                    .atoms,
                Quantity::from_decimal("3", 36).unwrap()
            );
            assert_eq!(
                prepared
                    .intent
                    .maximum_costs
                    .iter()
                    .find(|cost| cost.amount.asset.reference == "ZUSD")
                    .unwrap()
                    .amount
                    .atoms,
                Quantity::from_decimal("1", 36).unwrap()
            );
            assert!(revision.intent.expected.iter().any(|effect| matches!(effect,EffectConstraint::Debt { maximum_change,.. } if maximum_change == &Quantity::from_decimal("1030",36).unwrap())));
            assert!(revision.intent.expected.iter().any(|effect| matches!(effect,EffectConstraint::MarginPosition { maximum_delta,.. } if maximum_delta.magnitude() == Quantity::from_decimal("0.05",36).unwrap())));
            assert_eq!(
                prepared.valuation.as_ref().unwrap().movement_usd_micros,
                Quantity::new("230000000").unwrap()
            );
            assert_eq!(prepared.binding["amendment"]["batchOrderIndex"], 0);
            let refreshed = adapter.revalidate(&prepared).await.unwrap();
            assert_eq!(refreshed.binding, prepared.binding);
            assert!(
                !venue
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(endpoint, _)| endpoint.mutating())
            );
        }
    }
    #[tokio::test]
    async fn margin_batch_revision_checks_whole_extension_and_exact_original_member() {
        let (adapter, _, original) = fixture().await;
        let context = AmendmentContext {
            original: Some(original),
            ..Default::default()
        };
        let request = mutation(false, "0.03", Some("900"));
        assert_eq!(
            adapter
                .prepare_with_linked_context(
                    E::KrakenPrivatePostAmendOrder.operation(),
                    &request,
                    &Default::default(),
                    &context
                )
                .await
                .unwrap_err()
                .code,
            "KRAKEN_SPEND_BOUND_EXCEEDED"
        );
        let mut request = mutation(false, "0.03", None);
        request["txid"] = json!("UNOWNED");
        assert_eq!(
            adapter
                .prepare_with_linked_context(
                    E::KrakenPrivatePostAmendOrder.operation(),
                    &request,
                    &Default::default(),
                    &context
                )
                .await
                .unwrap_err()
                .code,
            "KRAKEN_AMENDMENT_TARGET_MISMATCH"
        );
    }

    #[tokio::test]
    async fn margin_batch_member_fee_admission_covers_untouched_members_in_each_possible_fee_unit()
    {
        for fees in [
            json!({"XXBT":"0.00015","ZUSD":"1.8"}),
            json!({"XXBT":"0.00009","ZUSD":"3"}),
        ] {
            let (adapter, venue, original) = fixture().await;
            let context = AmendmentContext {
                original: Some(original),
                ..Default::default()
            };
            let mut request = mutation(false, "0.03", Some("1030"));
            request["maximumFees"] = fees;
            // The selected member alone fits ($1.26 / 0.00006 XBT),
            // but the untouched member raises the command total to
            // $2.06 / 0.00010 XBT. Fee preferences guarantee neither unit.
            assert_eq!(
                adapter
                    .prepare_with_linked_context(
                        E::KrakenPrivatePostAmendOrder.operation(),
                        &request,
                        &Default::default(),
                        &context,
                    )
                    .await
                    .unwrap_err()
                    .code,
                "KRAKEN_FEE_BOUND_EXCEEDED"
            );
            assert!(
                !venue
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(endpoint, _)| endpoint.mutating())
            );
        }
    }

    struct Approval;
    #[async_trait]
    impl fin_domain::Authorizer for Approval {
        async fn authorize(
            &self,
            challenge: &fin_domain::AuthorizationChallenge,
        ) -> Result<fin_domain::ApprovalAudit> {
            Ok(fin_domain::ApprovalAudit {
                mechanism: "synthetic exact approval; not native Touch ID".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }
    struct Ledger(ExchangeAdapter<KrakenProfile>);
    #[async_trait]
    impl Adapter for Ledger {
        fn definitions(&self) -> Vec<fin_domain::Definition> {
            self.0.definitions()
        }
        async fn snapshot(&self) -> Result<fin_domain::StateSnapshot> {
            Ok(fin_domain::StateSnapshot {
                observed_at: now(),
                complete: true,
                pending: vec![],
                coverage: vec!["Synthetic exact custody; no live account".into()],
                state: fin_domain::FinancialState {
                    holdings: [("ZUSD", "10000"), ("XXBT", "1")]
                        .into_iter()
                        .map(|(code, quantity)| {
                            Ok(ResourceAmount {
                                resource: ResourceId("kraken:synthetic-margin-batch-edit".into()),
                                amount: Amount {
                                    asset: AssetId {
                                        network: "kraken".into(),
                                        reference: code.into(),
                                        decimals: 36,
                                    },
                                    atoms: Quantity::from_decimal(quantity, 36)?,
                                },
                            })
                        })
                        .collect::<Result<_>>()?,
                    ..Default::default()
                },
            })
        }
        async fn prepare(&self, o: &str, i: &Value) -> Result<PreparedOperation> {
            self.0.prepare(o, i).await
        }
        async fn prepare_with_linked_context(
            &self,
            o: &str,
            i: &Value,
            c: &fin_domain::PreparationContext,
            a: &AmendmentContext,
        ) -> Result<PreparedOperation> {
            self.0.prepare_with_linked_context(o, i, c, a).await
        }
        async fn revalidate(&self, p: &PreparedOperation) -> Result<PreparedOperation> {
            self.0.revalidate(p).await
        }
        async fn submit(
            &self,
            p: &PreparedOperation,
            j: Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<Submission> {
            self.0.submit(p, j).await
        }
        async fn reconcile(&self, r: &OperationRecord) -> Result<Reconciliation> {
            self.0.reconcile(r).await
        }
        async fn reconcile_with_context(
            &self,
            r: &OperationRecord,
            a: &AmendmentContext,
        ) -> Result<Reconciliation> {
            self.0.reconcile_with_context(r, a).await
        }
        async fn read(&self, o: &str, i: &Value) -> Result<Value> {
            self.0.read(o, i).await
        }
    }
    fn make_engine(path: &std::path::Path, venue: Arc<Venue>) -> fin_engine::Engine {
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic-margin-batch-edit".into()),
            credential_ref: "synthetic-key-only".into(),
            physical_identity: "SYNTHETIC-NOT-LIVE".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        let adapter = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], venue)
                .unwrap()
                .with_prices(Arc::new(Prices)),
        );
        fin_engine::Engine::new(
            fin_store::Store::open(path).unwrap(),
            vec![Arc::new(Ledger(adapter))],
            Arc::new(Approval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "10000000000".into(),
                max_run_allocation_usd_micros: "10000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    }
    #[tokio::test]
    async fn margin_member_revision_survives_sqlite_restart_without_second_native_amendment_or_fill_owner()
     {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.sqlite3");
        let venue = Arc::new(Venue::default());
        let engine = make_engine(&path, venue.clone());
        let original=engine.execute(fin_domain::OperationRequest{request_id:"original-margin-batch".into(),operation:E::KrakenPrivatePostAddOrderBatch.operation().into(),input:json!({"account":"spot","requestId":"original-margin-batch","pair":"XXBTZUSD","orders":[{"type":"buy","ordertype":"limit","price":"20000","volume":"0.02","leverage":"2"},{"type":"buy","ordertype":"limit","price":"20000","volume":"0.02","leverage":"2"}],"maximumFees":{"XXBT":"0.0001","ZUSD":"2"}}),run_id:None}).await.unwrap();
        assert_eq!(original.status, Status::Pending, "{:?}", original.error);
        let mut input = mutation(false, "0.03", Some("1030"));
        input["operationId"] = json!(original.id);
        input["originalSubmission"] = json!(original.submission);
        let request = fin_domain::OperationRequest {
            request_id: "margin-batch-member-revision".into(),
            operation: E::KrakenPrivatePostAmendOrder.operation().into(),
            input,
            run_id: None,
        };
        let amended = engine.execute(request.clone()).await.unwrap();
        assert_eq!(amended.status, Status::Completed, "{:?}", amended.error);
        assert!(
            amended
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .all(|e| matches!(e, FinancialEffect::SubmissionAmendment { .. }))
        );
        let owner_id = original.id.clone();
        drop(engine);
        let restarted = make_engine(&path, venue.clone());
        let owner = restarted.status(&owner_id, true).await.unwrap();
        assert_eq!(owner.status, Status::Pending, "{:?}", owner.error);
        assert_eq!(owner.submission, original.submission);
        assert_eq!(owner.preview, original.preview);
        let duplicate = restarted.execute(request).await.unwrap();
        assert_eq!(duplicate.id, amended.id);
        assert_eq!(
            venue
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(endpoint, _)| *endpoint == E::KrakenPrivatePostAmendOrder)
                .count(),
            1
        );
        let store = fin_store::Store::open(&path).unwrap();
        let revisions = store.amendment_revisions(&owner_id).unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].scope.binding["margin"]["orders"][1]["volume"],
            owner.preview.as_ref().unwrap().binding["margin"]["orders"][1]["volume"]
        );
    }
}
