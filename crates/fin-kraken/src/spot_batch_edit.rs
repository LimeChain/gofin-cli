//! One exact native child is revised while its original batch keeps all fills.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProfile,
    observation::{atoms, result, spot_credit_used},
    profile::error,
    trading::{SpotOrder, TradingControls},
};
use fin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, EffectConstraint,
    FinancialIntent, PreparedOperation, Quantity, ResourceAmount, Result,
    amendments::AmendmentScope,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
fn source(order: &SpotOrder) -> &AssetId {
    if order.buy { &order.quote } else { &order.base }
}
fn cost_key(cost: &CostBound) -> String {
    serde_json::to_string(&(&cost.location, &cost.amount.asset, &cost.treatment))
        .expect("typed exact key")
}
fn debit_totals(
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
fn cost_totals(rows: &[CostBound]) -> Result<BTreeMap<String, Quantity>> {
    let mut totals = BTreeMap::new();
    for row in rows {
        let total = totals.entry(cost_key(row)).or_insert_with(Quantity::zero);
        *total = total.checked_add(&row.amount.atoms)?;
    }
    Ok(totals)
}
impl KrakenProfile {
    pub(crate) async fn prepare_spot_batch_revision(
        &self,
        endpoint: E,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: Option<&AmendmentScope>,
    ) -> Result<PreparedOperation> {
        let previous = previous.ok_or_else(|| {
            error(
                "KRAKEN_BATCH_REVISION_OWNER_REQUIRED",
                "A batch revision requires its protected original operation and approved scope.",
            )
        })?;
        let binding = &previous.binding;
        if binding["conversionSequences"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
            || binding["conditionalClose"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        {
            let rows: Vec<SpotOrder> =
                serde_json::from_value(binding["orders"].clone()).map_err(|_| {
                    error(
                        "KRAKEN_AMENDMENT_TARGET_INVALID",
                        "Conditional original orders are unavailable.",
                    )
                })?;
            let parent_selected =
                binding["conditionalClose"]
                    .as_array()
                    .is_some_and(|conditions| {
                        conditions.iter().any(|condition| {
                            condition["parentIndex"]
                                .as_u64()
                                .and_then(|i| rows.get(i as usize))
                                .is_some_and(|parent| {
                                    parameters["txid"].as_str().is_some_and(|id| {
                                        parent.order_ids.iter().any(|known| known == id)
                                    }) || parameters["cl_ord_id"]
                                        .as_str()
                                        .is_some_and(|id| id == parent.client_id)
                                        || parameters["txid"].as_i64().is_some_and(|reference| {
                                            parent.user_reference.as_ref().and_then(Value::as_i64)
                                                == Some(reference)
                                        })
                                })
                        })
                    });
            if parent_selected {
                return Err(error("KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED","Kraken does not support atomic amendment or legacy edit of a parent with attached conditional-close terms. Preserve its original recovery operation; issued children require separate exact ownership and generated-credit proof.").with_details(json!({"source":"https://docs.kraken.com/exchange/guides/general/amends","legacySource":"https://docs.kraken.com/api-reference/trading/edit-order","venueUnsupported":true})));
            }
            return Err(error(
                "KRAKEN_BATCH_CONDITIONAL_REVISION_PENDING",
                "This composite conditional scope needs linked child quantity and generated-credit revision proof. Preserve the entry and use its original recovery operation.",
            ));
        }
        let mut orders: Vec<SpotOrder> = serde_json::from_value(binding["orders"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "The exact original batch orders are unavailable.",
                )
            })?;
        if orders.len() < 2
            || orders.len() > 15
            || previous.intent.expected.len() != orders.len()
            || !previous
                .intent
                .expected
                .iter()
                .all(|e| matches!(e, EffectConstraint::Conversion { .. }))
        {
            return Err(error(
                "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                "The protected batch needs one exact cash conversion per original native child.",
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
                "Native open-order ownership evidence is missing.",
            )
        })?;
        let selectors = &parameters;
        let candidates = orders
            .iter()
            .enumerate()
            .flat_map(|(index, order)| {
                rows.iter().filter_map(move |(id, row)| {
                    let owned = if order.order_ids.is_empty() {
                        row["cl_ord_id"] == order.client_id
                    } else {
                        order.order_ids.contains(id)
                    };
                    let selected = selectors["txid"]
                        .as_str()
                        .is_some_and(|target| target == id)
                        || selectors["cl_ord_id"]
                            .as_str()
                            .is_some_and(|target| target == order.client_id)
                        || selectors["txid"]
                            .as_i64()
                            .is_some_and(|target| row["userref"].as_i64() == Some(target));
                    (owned && selected).then_some(index)
                })
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Select exactly one open original batch child by its native order/client identity. A shared user reference cannot select multiple children.",
            ));
        }
        let selected = candidates[0];
        let old = &orders[selected];
        let mut minimums = previous
            .intent
            .expected
            .iter()
            .enumerate()
            .map(|(index, effect)| {
                binding["orderMinimumReceive"][index]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| match effect {
                        EffectConstraint::Conversion { minimum_output, .. } => {
                            minimum_output.to_decimal(36)
                        }
                        _ => "0".into(),
                    })
            })
            .collect::<Vec<_>>();
        let mut selected_controls = controls.clone();
        if selected_controls.minimum_receive.is_none()
            && !Quantity::from_decimal(&minimums[selected], 36)?.is_zero()
        {
            selected_controls.minimum_receive = Some(minimums[selected].clone());
        }
        minimums[selected] = selected_controls
            .minimum_receive
            .clone()
            .unwrap_or_else(|| "0".into());
        let mut virtual_binding = binding.clone();
        virtual_binding["orders"] = json!([old]);
        let mut native = binding["parameters"]["orders"][selected].clone();
        if !native.is_object() {
            return Err(error(
                "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                "The selected native original request is missing.",
            ));
        }
        native["pair"] = json!(old.pair);
        virtual_binding["parameters"] = native;
        let output = if old.buy { &old.base } else { &old.quote };
        let virtual_scope=AmendmentScope{binding:virtual_binding,valuation:previous.valuation.clone(),intent:FinancialIntent{debits:vec![ResourceAmount{resource:account.resource.clone(),amount:Amount{asset:source(old).clone(),atoms:old.maximum_spend.clone()}}],maximum_costs:previous.intent.maximum_costs.iter().filter(|cost|matches!(&cost.location,AccountingLocation::Cash{resource} if resource==&account.resource)&&((cost.amount.asset==*source(old)&&cost.treatment==CostTreatment::AdditionalDebit)||(cost.amount.asset==*output&&cost.treatment==CostTreatment::DeductedFromCredit))).cloned().collect(),expected:vec![previous.intent.expected[selected].clone()],locks:previous.intent.locks.clone()}};
        let mut prepared = match endpoint {
            E::KrakenPrivatePostAmendOrder => {
                self.prepare_spot_amendment(
                    input,
                    account,
                    parameters,
                    &selected_controls,
                    Some(&virtual_scope),
                )
                .await?
            }
            E::KrakenPrivatePostEditOrder => {
                self.prepare_spot_legacy_edit(
                    input,
                    account,
                    parameters,
                    &selected_controls,
                    Some(&virtual_scope),
                )
                .await?
            }
            _ => {
                return Err(error(
                    "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                    "This linked batch scope requires a documented atomic amendment or legacy edit.",
                ));
            }
        };
        let isolated = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment { revision, .. } => {
                    Some((**revision).clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                    "The selected child revision omitted its protected authority scope.",
                )
            })?;
        let revised: SpotOrder = serde_json::from_value(isolated.binding["orders"][0].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                    "Selected child revised quantity is unavailable.",
                )
            })?;
        let mut full = previous.intent.clone();
        full.expected[selected] = isolated.intent.expected.first().cloned().ok_or_else(|| {
            error(
                "KRAKEN_BATCH_REVISION_SCOPE_INVALID",
                "Selected child cash conversion is unavailable.",
            )
        })?;
        orders[selected] = revised;
        full.debits = orders
            .iter()
            .map(|order| ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: source(order).clone(),
                    atoms: order.maximum_spend.clone(),
                },
            })
            .collect();
        for cost in &mut full.maximum_costs {
            let maximum = controls
                .maximum_fees
                .get(&cost.amount.asset.reference)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_FEE_BOUNDS_REQUIRED",
                        "Every original batch fee currency must retain an exact approved ceiling.",
                    )
                })?;
            cost.amount.atoms = Quantity::from_decimal(maximum, 36)?;
        }
        if controls.maximum_fees.len()
            != orders
                .iter()
                .flat_map(|order| [order.base.reference.clone(), order.quote.reference.clone()])
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        {
            return Err(error(
                "INVALID_INPUT",
                "A batch revision cannot add an unrelated fee asset.",
            ));
        }
        self.verify_spot_fee_admission(account, &orders, controls)
            .await?;
        let old_debits = debit_totals(&previous.intent.debits)?;
        let new_debits = debit_totals(&full.debits)?;
        let old_costs = cost_totals(&previous.intent.maximum_costs)?;
        let new_costs = cost_totals(&full.maximum_costs)?;
        let mut delta = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for ((resource, asset), maximum) in &new_debits {
            let before = old_debits
                .get(&(resource.clone(), asset.clone()))
                .cloned()
                .unwrap_or_default();
            if maximum > &before {
                delta.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: maximum.checked_sub(&before)?,
                    },
                });
            }
        }
        for cost in &full.maximum_costs {
            let key = cost_key(cost);
            let before = old_costs.get(&key).cloned().unwrap_or_default();
            if new_costs[&key] > before {
                let mut increase = cost.clone();
                increase.amount.atoms = new_costs[&key].checked_sub(&before)?;
                if !delta.maximum_costs.iter().any(|old| cost_key(old) == key) {
                    delta.maximum_costs.push(increase);
                }
            }
        }
        let mut additional = BTreeMap::<AssetId, Quantity>::new();
        for row in &delta.debits {
            let total = additional.entry(row.amount.asset.clone()).or_default();
            *total = total.checked_add(&row.amount.atoms)?;
        }
        for cost in delta
            .maximum_costs
            .iter()
            .filter(|cost| cost.treatment == CostTreatment::AdditionalDebit)
        {
            let total = additional.entry(cost.amount.asset.clone()).or_default();
            *total = total.checked_add(&cost.amount.atoms)?;
        }
        let balance = self
            .json(E::KrakenPrivatePostBalanceEx, Some(account), json!({}))
            .await?;
        for (asset, needed) in additional {
            let row = &result(&balance)[&asset.reference];
            if !spot_credit_used(row)?.is_zero()
                || atoms(&row["balance"])?.checked_sub(&atoms(&row["hold_trade"])?)? < needed
            {
                return Err(error(
                    "INSUFFICIENT_BALANCE",
                    "Unborrowed free custody cannot cover the whole batch's newly approved principal and fee delta.",
                ));
            }
        }
        let mut full_binding = binding.clone();
        full_binding["orders"] = json!(orders);
        let mut revised_native = isolated.binding["parameters"].clone();
        revised_native.as_object_mut().unwrap().remove("pair");
        full_binding["parameters"]["orders"][selected] = revised_native;
        full_binding["controls"] = json!(controls);
        full_binding["controls"]["minimumReceive"] = binding["controls"]["minimumReceive"].clone();
        full_binding["orderMinimumReceive"] = json!(minimums);
        full_binding
            .as_object_mut()
            .unwrap()
            .remove(fin_domain::value_bounds::KEY);
        let revision = AmendmentScope {
            intent: full.clone(),
            binding: full_binding,
            valuation: Some(
                self.financial_valuation(&full.debits, &full.maximum_costs)
                    .await?,
            ),
        };
        let marker = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment {
                    operation_id,
                    submission,
                    ..
                } => Some((operation_id.clone(), submission.clone())),
                _ => None,
            })
            .unwrap();
        delta.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: marker.0,
            submission: marker.1,
            revision: Box::new(revision),
        });
        prepared.intent = delta;
        prepared.valuation = Some(
            self.financial_valuation(&prepared.intent.debits, &prepared.intent.maximum_costs)
                .await?,
        );
        prepared.binding["amendmentPreviousScope"] = json!(previous);
        prepared.binding["amendment"]["batchOrderIndex"] = json!(selected);
        prepared.summary = format!(
            "Revise original Kraken batch child {} on {}; reserve the exact additional principal and fees under its original fill owner.",
            selected + 1,
            account.name
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
    use fin_domain::{Adapter, FinError, ResourceId, Submission, now};
    use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
    use fin_pricing::{UsdPriceSource, UsdQuote};
    use std::sync::{Arc, Mutex};
    pub(super) struct Prices;
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
                source: "synthetic exact native assets".into(),
            })
        }
    }
    pub(super) struct Venue {
        pub(super) calls: Mutex<Vec<(E, Value)>>,
    }
    #[async_trait]
    impl KrakenTransport for Venue {
        async fn request(
            &self,
            e: E,
            _: Option<&KrakenAccount>,
            p: Value,
            g: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(g.is_none(), "Preview must not dispatch a native write");
            self.calls.lock().unwrap().push((e, p));
            let value = match e {
                E::KrakenPrivatePostGetApiKeyInfo => {
                    json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0"}})
                }
                E::KrakenPublicGetAssets => {
                    json!({"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
                }
                E::KrakenPublicGetAssetPairs => {
                    json!({"result":{"XXBTZUSD":{"altname":"XBTUSD","base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"pair_decimals":1,"cost_decimals":5,"ordermin":"0.0001"}}})
                }
                E::KrakenPrivatePostTradeVolume => {
                    json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.4"}},"fees_maker":{"XXBTZUSD":{"fee":"0.25"}}}})
                }
                E::KrakenPrivatePostBalanceEx => {
                    json!({"result":{"XXBT":{"balance":"1","hold_trade":"0","credit_used":"0"},"ZUSD":{"balance":"1000","hold_trade":"0","credit_used":"0"}}})
                }
                E::KrakenPrivatePostOpenOrders => {
                    json!({"result":{"open":{"O1":{"cl_ord_id":"batch1","userref":777,"descr":{"pair":"XXBTZUSD","type":"buy","price":"20000"},"status":"open","vol":"0.01","vol_exec":"0.005","cost":"100","fee":"0.2","trades":["T1"]},"O2":{"cl_ord_id":"batch2","userref":777,"descr":{"pair":"XXBTZUSD","type":"buy","price":"20000"},"status":"open","vol":"0.01","vol_exec":"0.005","cost":"100","fee":"0.2","trades":["T2"]}}}})
                }
                _ => {
                    return Err(FinError::validation(format!(
                        "Unexpected synthetic batch revision read {e:?}"
                    )));
                }
            };
            Ok(KrakenResponse::Json(value))
        }
    }
    async fn setup() -> (
        ExchangeAdapter<KrakenProfile>,
        AmendmentScope,
        Submission,
        Arc<Venue>,
    ) {
        let venue = Arc::new(Venue {
            calls: Mutex::new(vec![]),
        });
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic-batch".into()),
            credential_ref: "synthetic-only".into(),
            physical_identity: "synthetic-batch".into(),
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
        let input = json!({"account":"spot","requestId":"synthetic-original-batch","pair":"XXBTZUSD","orders":[{"cl_ord_id":"batch1","type":"buy","ordertype":"limit","price":"20000","volume":"0.01"},{"cl_ord_id":"batch2","type":"buy","ordertype":"limit","price":"20000","volume":"0.01"}],"maximumFees":{"XXBT":"0.0001","ZUSD":"3"}});
        let original = adapter
            .prepare(E::KrakenPrivatePostAddOrderBatch.operation(), &input)
            .await
            .unwrap();
        let scope = AmendmentScope {
            intent: original.intent,
            binding: original.binding,
            valuation: original.valuation,
        };
        let submission = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "synthetic-batch-submission".into(),
            data: json!({"binding":scope.binding,"submittedAt":now()}),
        };
        (adapter, scope, submission, venue)
    }
    fn linked(scope: &AmendmentScope) -> fin_domain::amendments::AmendmentContext {
        fin_domain::amendments::AmendmentContext {
            original_evidence: None,
            original: Some(PreparedOperation {
                operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
                input: json!({}),
                binding: scope.binding.clone(),
                summary: "Synthetic protected original".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: now(),
                expires_at: now(),
                valuation: scope.valuation.clone(),
                intent: scope.intent.clone(),
                action_deadline: None,
                operation_version: 1,
            }),
            revisions: vec![],
        }
    }
    #[tokio::test]
    async fn atomic_and_legacy_batch_child_revisions_preserve_other_order_and_reserve_whole_scope_delta()
     {
        for legacy in [false, true] {
            let (adapter, scope, submission, _) = setup().await;
            let untouched = scope.binding["orders"][1].clone();
            let mut input = json!({"account":"spot","requestId":if legacy{"synthetic-legacy-child"}else{"synthetic-atomic-child"},"operationId":"synthetic-original-id","originalSubmission":submission,"txid":"O1","maximumFees":{"XXBT":"0.0002","ZUSD":"4"}});
            let endpoint = if legacy {
                input["pair"] = json!("XXBTZUSD");
                input["volume"] = json!("0.02");
                input["price"] = json!("21000");
                E::KrakenPrivatePostEditOrder
            } else {
                input["order_qty"] = json!("0.02");
                input["limit_price"] = json!("21000");
                E::KrakenPrivatePostAmendOrder
            };
            let prepared = adapter
                .prepare_with_linked_context(
                    endpoint.operation(),
                    &input,
                    &fin_domain::PreparationContext::default(),
                    &linked(&scope),
                )
                .await
                .unwrap();
            let revision = prepared
                .intent
                .expected
                .iter()
                .find_map(|e| match e {
                    EffectConstraint::SubmissionAmendment { revision, .. } => Some(revision),
                    _ => None,
                })
                .unwrap();
            assert_eq!(revision.binding["orders"][1], untouched);
            assert_eq!(
                revision.binding["parameters"]["orders"][1],
                scope.binding["parameters"]["orders"][1]
            );
            assert_eq!(revision.intent.expected[1], scope.intent.expected[1]);
            assert_eq!(revision.binding["orders"].as_array().unwrap().len(), 2);
            let full = revision
                .intent
                .debits
                .iter()
                .try_fold(Quantity::zero(), |total, row| {
                    total.checked_add(&row.amount.atoms)
                })
                .unwrap();
            assert_eq!(
                full,
                Quantity::from_decimal(if legacy { "720" } else { "615" }, 36).unwrap()
            );
            assert_eq!(
                prepared.intent.debits[0].amount.atoms,
                Quantity::from_decimal(if legacy { "320" } else { "215" }, 36).unwrap()
            );
            assert_eq!(prepared.binding["amendment"]["batchOrderIndex"], 0);
            assert_eq!(prepared.binding["amendmentPreviousScope"], json!(scope));
            assert_eq!(
                revision.intent.maximum_costs.len(),
                scope.intent.maximum_costs.len()
            );
            assert!(
                prepared
                    .intent
                    .maximum_costs
                    .iter()
                    .any(|cost| cost.treatment == CostTreatment::AdditionalDebit
                        && cost.amount.atoms == Quantity::from_decimal("1", 36).unwrap())
            );
            let checked = adapter.revalidate(&prepared).await.unwrap();
            assert_eq!(checked.intent, prepared.intent);
            assert_eq!(checked.binding, prepared.binding);
        }
    }
    #[tokio::test]
    async fn batch_child_receipt_floor_is_targeted_and_preserved_when_omitted() {
        let (adapter, mut scope, mut submission, _) = setup().await;
        scope.binding["orderMinimumReceive"] = json!(["0.004", "0.005"]);
        for (index, minimum) in ["0.004", "0.005"].iter().enumerate() {
            if let EffectConstraint::Conversion { minimum_output, .. } =
                &mut scope.intent.expected[index]
            {
                *minimum_output = Quantity::from_decimal(minimum, 36).unwrap();
            }
        }
        submission.data["binding"] = scope.binding.clone();
        for requested in [None, Some("0.006")] {
            let mut input = json!({"account":"spot","requestId":"synthetic-minimum-child","operationId":"synthetic-original-id","originalSubmission":submission,"txid":"O1","order_qty":"0.02","limit_price":"21000","maximumFees":{"XXBT":"0.0002","ZUSD":"4"}});
            if let Some(minimum) = requested {
                input["minimumReceive"] = json!(minimum);
            }
            let prepared = adapter
                .prepare_with_linked_context(
                    E::KrakenPrivatePostAmendOrder.operation(),
                    &input,
                    &fin_domain::PreparationContext::default(),
                    &linked(&scope),
                )
                .await
                .unwrap();
            let revision = prepared
                .intent
                .expected
                .iter()
                .find_map(|e| match e {
                    EffectConstraint::SubmissionAmendment { revision, .. } => Some(revision),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                revision.binding["orderMinimumReceive"],
                json!([requested.unwrap_or("0.004"), "0.005"])
            );
            assert_eq!(revision.intent.expected[1], scope.intent.expected[1]);
            assert!(revision.binding["controls"]["minimumReceive"].is_null());
            assert!(
                matches!(&revision.intent.expected[0],EffectConstraint::Conversion{minimum_output,..} if minimum_output==&Quantity::from_decimal(requested.unwrap_or("0.004"),36).unwrap())
            );
            let checked = adapter.revalidate(&prepared).await.unwrap();
            assert_eq!(checked.intent, prepared.intent);
            assert_eq!(checked.binding, prepared.binding);
        }
    }
    #[tokio::test]
    async fn batch_revision_cannot_select_unowned_or_shared_reference_children() {
        let (adapter, scope, submission, venue) = setup().await;
        for target in [json!("UNRELATED"), json!(777)] {
            let legacy = target.is_number();
            let mut input = json!({"account":"spot","requestId":"synthetic-wrong-child","operationId":"synthetic-original-id","originalSubmission":submission,"txid":target,"maximumFees":{"XXBT":"0.0002","ZUSD":"4"}});
            let endpoint = if legacy {
                input["pair"] = json!("XXBTZUSD");
                input["price"] = json!("21000");
                E::KrakenPrivatePostEditOrder
            } else {
                input["limit_price"] = json!("21000");
                E::KrakenPrivatePostAmendOrder
            };
            assert_eq!(
                adapter
                    .prepare_with_linked_context(
                        endpoint.operation(),
                        &input,
                        &fin_domain::PreparationContext::default(),
                        &linked(&scope)
                    )
                    .await
                    .unwrap_err()
                    .code,
                "KRAKEN_AMENDMENT_TARGET_MISMATCH"
            );
        }
        assert!(venue.calls.lock().unwrap().iter().all(|(e, _)| !matches!(
            e,
            E::KrakenPrivatePostAmendOrder | E::KrakenPrivatePostEditOrder
        )));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use async_trait::async_trait;
    use fin_domain::{
        Adapter, ApprovalAudit, AuthorizationChallenge, Authorizer, Definition, FinancialEffect,
        FinancialState, OperationRecord, OperationRequest, Reconciliation, ResourceId,
        StateSnapshot, Status, Submission, SubmissionJournal, now,
    };
    use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Venue {
        reads: super::tests::Venue,
        legacy: AtomicBool,
        amended: AtomicBool,
        finished: AtomicBool,
        writes: Mutex<Vec<E>>,
    }
    impl Venue {
        fn order(&self, id: &str) -> Value {
            let legacy = self.legacy.load(Ordering::SeqCst);
            let finished = self.finished.load(Ordering::SeqCst);
            let amended = self.amended.load(Ordering::SeqCst);
            let first = id == "O1";
            let successor = id == "NEW";
            let initial = first && legacy;
            let quantity = if successor || first && amended {
                "0.02"
            } else {
                "0.01"
            };
            let executed = if initial {
                "0.005"
            } else if successor {
                if finished { "0.02" } else { "0" }
            } else if finished {
                quantity
            } else {
                "0.005"
            };
            let cost = if initial {
                "100"
            } else if successor {
                if finished { "420" } else { "0" }
            } else if finished && first && amended {
                "415"
            } else if finished {
                "200"
            } else {
                "100"
            };
            let fee = match cost {
                "420" => "0.84",
                "415" => "0.83",
                "200" => "0.4",
                "100" => "0.2",
                _ => "0",
            };
            let trades = if successor {
                if finished { json!(["TS"]) } else { json!([]) }
            } else if first {
                if finished && !initial {
                    json!(["T1", "T3"])
                } else {
                    json!(["T1"])
                }
            } else if finished {
                json!(["T2", "T4"])
            } else {
                json!(["T2"])
            };
            json!({"opentm":format!("{}.{:09}",chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp_subsec_nanos()),"cl_ord_id":if successor{"batch-legacy"}else if first{"batch1"}else{"batch2"},"descr":{"pair":"XXBTZUSD","type":"buy","ordertype":"limit","price":if successor||first&&amended&&!legacy{"21000"}else{"20000"}},"status":if initial{"canceled"}else if finished{"closed"}else{"open"},"vol":quantity,"vol_exec":executed,"cost":cost,"fee":fee,"trades":trades})
        }
        fn fill(id: &str) -> (&'static str, &'static str, &'static str, &'static str) {
            match id {
                "T1" => ("O1", "0.005", "100", "0.2"),
                "T2" => ("O2", "0.005", "100", "0.2"),
                "T3" => ("O1", "0.015", "315", "0.63"),
                "T4" => ("O2", "0.005", "100", "0.2"),
                "TS" => ("NEW", "0.02", "420", "0.84"),
                _ => panic!("unknown synthetic trade"),
            }
        }
    }
    #[async_trait]
    impl KrakenTransport for Venue {
        async fn request(
            &self,
            e: E,
            a: Option<&KrakenAccount>,
            p: Value,
            g: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            let value = match e {
                E::KrakenPrivatePostAddOrderBatch => {
                    assert!(g.unwrap()());
                    self.writes.lock().unwrap().push(e);
                    json!({"result":{"orders":[{"txid":"O1"},{"txid":"O2"}]}})
                }
                E::KrakenPrivatePostAmendOrder => {
                    assert!(g.unwrap()());
                    assert_eq!(p["txid"], "O1");
                    self.writes.lock().unwrap().push(e);
                    self.amended.store(true, Ordering::SeqCst);
                    json!({"result":{"amend_id":"AMEND"}})
                }
                E::KrakenPrivatePostEditOrder => {
                    assert!(g.unwrap()());
                    assert_eq!(p["txid"], "O1");
                    self.writes.lock().unwrap().push(e);
                    self.amended.store(true, Ordering::SeqCst);
                    self.legacy.store(true, Ordering::SeqCst);
                    json!({"result":{"txid":"NEW","originaltxid":"O1","status":"Ok","orders_cancelled":1}})
                }
                E::KrakenPrivatePostOrderAmends => {
                    json!({"result":{"amends":[{"amend_id":"AMEND","order_qty":"0.02","limit_price":"21000"}]}})
                }
                E::KrakenPrivatePostOpenOrders | E::KrakenPrivatePostClosedOrders => {
                    let ids = if self.legacy.load(Ordering::SeqCst) {
                        vec!["O1", "O2", "NEW"]
                    } else {
                        vec!["O1", "O2"]
                    };
                    let mut rows = serde_json::Map::new();
                    for id in ids {
                        let row = self.order(id);
                        if (row["status"] == "open") == (e == E::KrakenPrivatePostOpenOrders) {
                            rows.insert(id.into(), row);
                        }
                    }
                    if e == E::KrakenPrivatePostOpenOrders {
                        json!({"result":{"open":rows}})
                    } else {
                        json!({"result":{"count":rows.len(),"closed":rows}})
                    }
                }
                E::KrakenPrivatePostQueryOrders => {
                    let mut rows = serde_json::Map::new();
                    for id in p["txid"].as_str().unwrap().split(',') {
                        rows.insert(id.into(), self.order(id));
                    }
                    json!({"result":rows})
                }
                E::KrakenPrivatePostQueryTrades => {
                    let mut rows = serde_json::Map::new();
                    for id in p["txid"].as_str().unwrap().split(',') {
                        let (order, volume, cost, fee) = Self::fill(id);
                        rows.insert(id.into(),json!({"ordertxid":order,"pair":"XXBTZUSD","type":"buy","vol":volume,"cost":cost,"fee":fee,"margin":"0","ledgers":[format!("BASE-{id}"),format!("QUOTE-{id}")]}));
                    }
                    json!({"result":rows})
                }
                E::KrakenPrivatePostQueryLedgers => {
                    let mut rows = serde_json::Map::new();
                    for key in p["id"].as_str().unwrap().split(',') {
                        let (base, id) = if let Some(id) = key.strip_prefix("BASE-") {
                            (true, id)
                        } else {
                            (false, key.strip_prefix("QUOTE-").unwrap())
                        };
                        let (_, volume, cost, fee) = Self::fill(id);
                        rows.insert(key.into(),json!({"refid":id,"type":"trade","asset":if base{"XXBT"}else{"ZUSD"},"amount":if base{volume.into()}else{format!("-{cost}")},"fee":if base{"0"}else{fee}}));
                    }
                    json!({"result":rows})
                }
                _ => return self.reads.request(e, a, p, g).await,
            };
            Ok(KrakenResponse::Json(value))
        }
    }
    struct Approval;
    #[async_trait]
    impl Authorizer for Approval {
        async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                mechanism: "synthetic exact approval; not Touch ID".into(),
                binding_hash: c.binding_hash.clone(),
                challenge_nonce: c.nonce.clone(),
                approved_at: now(),
                expires_at: c.expires_at.clone(),
            })
        }
    }
    struct Observed {
        venue: ExchangeAdapter<KrakenProfile>,
    }
    #[async_trait]
    impl Adapter for Observed {
        async fn read(&self, o: &str, i: &Value) -> Result<Value> {
            self.venue.read(o, i).await
        }
        fn definitions(&self) -> Vec<Definition> {
            self.venue.definitions()
        }
        async fn snapshot(&self) -> Result<StateSnapshot> {
            Ok(StateSnapshot {
                observed_at: now(),
                complete: true,
                pending: vec![],
                coverage: vec!["Synthetic exact fixture holdings".into()],
                state: FinancialState {
                    holdings: [("XXBT", "1"), ("ZUSD", "1000")]
                        .into_iter()
                        .map(|(asset, n)| ResourceAmount {
                            resource: ResourceId("kraken:synthetic-batch".into()),
                            amount: Amount {
                                asset: AssetId {
                                    network: "kraken".into(),
                                    reference: asset.into(),
                                    decimals: 36,
                                },
                                atoms: Quantity::from_decimal(n, 36).unwrap(),
                            },
                        })
                        .collect(),
                    ..Default::default()
                },
            })
        }
        async fn prepare(&self, o: &str, i: &Value) -> Result<PreparedOperation> {
            self.venue.prepare(o, i).await
        }
        async fn prepare_with_linked_context(
            &self,
            o: &str,
            i: &Value,
            c: &fin_domain::PreparationContext,
            l: &fin_domain::amendments::AmendmentContext,
        ) -> Result<PreparedOperation> {
            self.venue.prepare_with_linked_context(o, i, c, l).await
        }
        async fn revalidate(&self, p: &PreparedOperation) -> Result<PreparedOperation> {
            self.venue.revalidate(p).await
        }
        async fn submit(
            &self,
            p: &PreparedOperation,
            j: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            self.venue.submit(p, j).await
        }
        async fn reconcile(&self, r: &OperationRecord) -> Result<Reconciliation> {
            self.venue.reconcile(r).await
        }
        async fn reconcile_with_context(
            &self,
            r: &OperationRecord,
            c: &fin_domain::amendments::AmendmentContext,
        ) -> Result<Reconciliation> {
            self.venue.reconcile_with_context(r, c).await
        }
    }
    fn engine(path: &std::path::Path, fixture: Arc<Venue>) -> fin_engine::Engine {
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic-batch".into()),
            credential_ref: "synthetic-only".into(),
            physical_identity: "synthetic-batch".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        let adapter = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], fixture)
                .unwrap()
                .with_prices(Arc::new(super::tests::Prices)),
        );
        fin_engine::Engine::new(
            fin_store::Store::open(path).unwrap(),
            vec![Arc::new(Observed { venue: adapter })],
            Arc::new(Approval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "1000000000".into(),
                max_run_allocation_usd_micros: "1000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    }
    #[tokio::test]
    async fn original_cash_batch_remains_only_fill_owner_across_atomic_or_legacy_revision_and_restart()
     {
        for legacy in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "fin-spot-batch-revision-{}.sqlite3",
                uuid::Uuid::new_v4()
            ));
            let fixture = Arc::new(Venue {
                reads: super::tests::Venue {
                    calls: Mutex::new(vec![]),
                },
                legacy: AtomicBool::new(false),
                amended: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                writes: Mutex::new(vec![]),
            });
            let e = engine(&path, fixture.clone());
            let request = OperationRequest {
                request_id: "synthetic-native-original-batch".into(),
                operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
                run_id: None,
                input: json!({"account":"spot","requestId":"synthetic-native-original-batch","pair":"XXBTZUSD","orders":[{"cl_ord_id":"batch1","type":"buy","ordertype":"limit","price":"20000","volume":"0.01"},{"cl_ord_id":"batch2","type":"buy","ordertype":"limit","price":"20000","volume":"0.01"}],"maximumFees":{"XXBT":"0.0001","ZUSD":"3"}}),
            };
            let original = e.execute(request.clone()).await.unwrap();
            assert_eq!(original.status, Status::Pending, "{original:#?}");
            assert!(original.effects.is_some(), "{original:#?}");
            let mut input = json!({"account":"spot","requestId":"synthetic-native-batch-revision","operationId":original.id,"originalSubmission":original.submission,"txid":"O1","maximumFees":{"XXBT":"0.0002","ZUSD":"4"}});
            let endpoint = if legacy {
                input["pair"] = json!("XXBTZUSD");
                input["volume"] = json!("0.02");
                input["price"] = json!("21000");
                E::KrakenPrivatePostEditOrder
            } else {
                input["order_qty"] = json!("0.02");
                input["limit_price"] = json!("21000");
                E::KrakenPrivatePostAmendOrder
            };
            let changed = e
                .execute(OperationRequest {
                    request_id: "synthetic-native-batch-revision".into(),
                    operation: endpoint.operation().into(),
                    input,
                    run_id: None,
                })
                .await
                .unwrap();
            assert_eq!(changed.status, Status::Completed, "{:#?}", changed.error);
            assert!(
                changed
                    .effects
                    .as_ref()
                    .unwrap()
                    .effects
                    .iter()
                    .all(|effect| matches!(effect, FinancialEffect::SubmissionAmendment { .. }))
            );
            drop(e);
            fixture.finished.store(true, Ordering::SeqCst);
            let e = engine(&path, fixture.clone());
            let final_record = e.status(&original.id, true).await.unwrap();
            assert_eq!(
                final_record.status,
                Status::Completed,
                "{:#?}",
                final_record.error
            );
            assert_eq!(final_record.preview, original.preview);
            assert_eq!(final_record.submission, original.submission);
            let spent = final_record
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    FinancialEffect::Conversion { spent, .. } => Some(spent.atoms.clone()),
                    _ => None,
                })
                .try_fold(Quantity::zero(), |a, b| a.checked_add(&b))
                .unwrap();
            assert_eq!(
                spent,
                Quantity::from_decimal(if legacy { "720" } else { "615" }, 36).unwrap()
            );
            assert_eq!(e.execute(request).await.unwrap().id, original.id);
            assert_eq!(fixture.writes.lock().unwrap().len(), 2);
            drop(e);
            std::fs::remove_file(path).unwrap();
        }
    }
}
