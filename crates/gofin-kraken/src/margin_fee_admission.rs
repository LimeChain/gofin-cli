//! Current trading fee admission is independent of declared fee reservations.
//! TradeVolume does not publish the dynamic margin opening/rollover schedule.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProfile,
    observation::{atoms, exact, result},
    profile::error,
};
use gofin_domain::{AssetId, Quantity, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One full native order scope, including each generated conditional child.
/// Quote notional is an admission estimate for standing sells, never a venue cap.
#[derive(Clone, Debug)]
pub(crate) struct MarginFeeScope {
    pub(crate) pair: String,
    pub(crate) base: AssetId,
    pub(crate) quote: AssetId,
    pub(crate) volume: Quantity,
    pub(crate) quote_notional: Quantity,
    pub(crate) settling: bool,
}
const DISCLOSURE: &str = "Current authenticated trading fees are estimated in both traded assets and aggregated across the whole command. Kraken fee-currency flags are preferences, and standing-order execution prices and rates may change. The REST schemas do not expose current dynamic margin opening/rollover rates. Declared caps reserve and constrain proven command costs; they do not guarantee a finite ceiling on future venue rollover, liquidation or standing-position liability. GoFin never silently renews a timer or closes a position.";

impl KrakenProfile {
    pub(crate) async fn margin_fee_scope(
        &self,
        mut scope: MarginFeeScope,
        native: &Value,
    ) -> Result<MarginFeeScope> {
        let pair = &scope.pair;
        let volume = &scope.volume;
        let settling = scope.settling;
        let mut reference = Quantity::zero();
        if !settling {
            let ticker = self
                .json(E::KrakenPublicGetTicker, None, json!({"pair":pair}))
                .await?;
            let row = result(&ticker).get(pair).ok_or_else(|| {
                error(
                    "KRAKEN_FEE_PRICE_PENDING",
                    "Current margin quote fee estimate needs the exact native market ticker.",
                )
            })?;
            for side in ["a", "b"] {
                if let Some(price) = row[side].as_array().and_then(|values| values.first()) {
                    reference = reference.max(atoms(price)?);
                }
            }
            for field in ["price", "price2", "limit_price"] {
                if let Some(value) = native.get(field)
                    && let Ok(text) = exact(value)
                    && !text.starts_with(['+', '-'])
                    && !text.ends_with('%')
                    && let Ok(price) = atoms(value)
                {
                    reference = reference.max(price);
                }
            }
            if reference.is_zero() {
                return Err(error(
                    "KRAKEN_FEE_PRICE_PENDING",
                    "Margin fee admission requires a positive exact native quote-price observation.",
                ));
            }
        }
        scope.quote_notional =
            volume.mul_div_ceil(&reference, &Quantity::new(format!("1{}", "0".repeat(36)))?)?;
        Ok(scope)
    }

    /// Check the entire native command/revised owner scope, never each child
    /// independently against a reused whole-command ceiling.
    pub(crate) async fn verify_margin_fee_admission(
        &self,
        account: &KrakenAccount,
        scopes: &[MarginFeeScope],
        maximum_fees: &BTreeMap<String, String>,
    ) -> Result<Value> {
        let mut rates: BTreeMap<String, Quantity> = BTreeMap::new();
        let mut needed: BTreeMap<String, Quantity> = BTreeMap::new();
        let mut markets = BTreeMap::new();
        for scope in scopes {
            if scope.base.decimals != 36 || scope.quote.decimals != 36 || scope.base == scope.quote
            {
                return Err(error(
                    "KRAKEN_FEE_ASSET_SCOPE_INVALID",
                    "Margin fee admission requires distinct exact canonical native assets with GoFin exchange quantity units.",
                ));
            }
            if scope.settling {
                continue;
            }
            if scope.volume.is_zero() || scope.quote_notional.is_zero() {
                return Err(error(
                    "KRAKEN_FEE_ASSET_SCOPE_INVALID",
                    "Trading fee admission needs the full positive native base quantity and independently observed quote notional.",
                ));
            }
            if !rates.contains_key(&scope.pair) {
                let response = self
                    .json(
                        E::KrakenPrivatePostTradeVolume,
                        Some(account),
                        json!({"pair":scope.pair,"fee_schedule":true}),
                    )
                    .await?;
                let data = result(&response);
                let taker = data["fees"][&scope.pair].get("fee").ok_or_else(|| {
                    error(
                        "KRAKEN_FEE_PROOF_PENDING",
                        "Current protected fee service omitted the exact margin market taker rate.",
                    )
                })?;
                let text = exact(taker)?;
                if text.starts_with('-') {
                    return Err(error(
                        "KRAKEN_FEE_PROOF_PENDING",
                        "The current native taker fee must be a nonnegative percentage.",
                    ));
                }
                let mut rate = Quantity::from_decimal(text.strip_prefix('+').unwrap_or(&text), 36)?;
                let mut maker_evidence = Value::Null;
                if let Some(maker) = data["fees_maker"][&scope.pair].get("fee") {
                    let text = exact(maker)?;
                    let magnitude = Quantity::from_decimal(
                        text.strip_prefix('-')
                            .or_else(|| text.strip_prefix('+'))
                            .unwrap_or(&text),
                        36,
                    )?;
                    if !text.starts_with('-') {
                        rate = rate.max(magnitude);
                    }
                    maker_evidence = json!(text);
                }
                markets.insert(scope.pair.clone(),json!({"takerPercent":exact(taker)?,"makerPercent":maker_evidence,"admittedPercent":rate.to_decimal(36),"source":"authenticated Spot GetTradeVolume","feeCurrencyFlags":"preferences; both native units checked"}));
                rates.insert(scope.pair.clone(), rate);
            }
            let rate = &rates[&scope.pair];
            let divisor = Quantity::new(format!("100{}", "0".repeat(36)))?;
            for (asset, principal) in [
                (&scope.base, &scope.volume),
                (&scope.quote, &scope.quote_notional),
            ] {
                let charge = principal.mul_div_ceil(rate, &divisor)?;
                let total = needed.entry(asset.reference.clone()).or_default();
                *total = total.checked_add(&charge)?;
            }
        }
        for (asset, estimate) in &needed {
            let cap = maximum_fees.get(asset).ok_or_else(|| error("KRAKEN_FEE_BOUNDS_REQUIRED", "Declare whole-command maximumFees for both exact canonical traded assets; fcib/fciq are preferences and do not guarantee the charged currency."))?;
            if *estimate > Quantity::from_decimal(cap, 36)? {
                return Err(error(
                    "KRAKEN_FEE_BOUND_EXCEEDED",
                    "Current aggregate native trading fee estimate exceeds the approved whole-command fee asset ceiling.",
                ));
            }
        }
        Ok(
            json!({"schemaVersion":1,"markets":markets,"tradingEstimateByAsset":needed.iter().map(|(asset,amount)|(asset.clone(),json!(amount.to_decimal(36)))).collect::<serde_json::Map<String,Value>>(),"openingRate":{"status":"unavailable_in_pinned_rest_schema","basis":"full native loan principal, not net collateral","creditLineFeesUsed":false},"futureRollover":{"status":"standing_venue_liability_not_finite_command_guarantee","automaticRenewal":false},"disclosure":DISCLOSURE}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use async_trait::async_trait;
    use gofin_domain::ResourceId;
    use gofin_exchange::kraken::DispatchGuard;
    use std::sync::{Arc, Mutex};
    struct Venue {
        taker: &'static str,
        maker: Option<&'static str>,
        missing_pair: bool,
        calls: Mutex<Vec<(E, Value)>>,
    }
    #[async_trait]
    impl KrakenTransport for Venue {
        async fn request(
            &self,
            endpoint: E,
            account: Option<&KrakenAccount>,
            parameters: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(!endpoint.mutating());
            assert!(guard.is_none());
            self.calls
                .lock()
                .unwrap()
                .push((endpoint, parameters.clone()));
            let value = if endpoint == E::KrakenPrivatePostTradeVolume {
                assert_eq!(account.unwrap().name, "synthetic-margin");
                assert_eq!(parameters, json!({"pair":"XXBTZUSD","fee_schedule":true}));
                let pair = if self.missing_pair {
                    "OTHER"
                } else {
                    "XXBTZUSD"
                };
                let mut fees = json!({"fees":{pair:{"fee":self.taker}}});
                if let Some(maker) = self.maker {
                    fees["fees_maker"] = json!({pair:{"fee":maker}});
                }
                json!({"result":fees})
            } else if endpoint == E::KrakenPublicGetTicker {
                assert!(account.is_none());
                json!({"result":{"XXBTZUSD":{"a":["20000"],"b":["19999"]}}})
            } else {
                panic!("unexpected synthetic fee endpoint {endpoint:?}")
            };
            Ok(KrakenResponse::Json(value))
        }
    }
    fn account() -> KrakenAccount {
        KrakenAccount {
            name: "synthetic-margin".into(),
            resource: ResourceId("synthetic-margin-custody".into()),
            credential_ref: "synthetic-unusable-key".into(),
            physical_identity: "synthetic-margin-identity".into(),
            product: KrakenProduct::Spot,
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
        }
    }
    fn profile(
        taker: &'static str,
        maker: Option<&'static str>,
        missing_pair: bool,
    ) -> (KrakenProfile, Arc<Venue>) {
        let venue = Arc::new(Venue {
            taker,
            maker,
            missing_pair,
            calls: Mutex::new(vec![]),
        });
        (
            KrakenProfile::new(KrakenProduct::Spot, vec![account()], venue.clone()).unwrap(),
            venue,
        )
    }
    fn scope() -> MarginFeeScope {
        MarginFeeScope {
            pair: "XXBTZUSD".into(),
            base: AssetId {
                network: "kraken".into(),
                reference: "XXBT".into(),
                decimals: 36,
            },
            quote: AssetId {
                network: "kraken".into(),
                reference: "ZUSD".into(),
                decimals: 36,
            },
            volume: Quantity::from_decimal("0.01", 36).unwrap(),
            quote_notional: Quantity::from_decimal("200", 36).unwrap(),
            settling: false,
        }
    }
    fn caps(base: &str, quote: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("XXBT".into(), base.into()), ("ZUSD".into(), quote.into())])
    }
    #[tokio::test]
    async fn whole_batch_aggregates_each_native_fee_cap_and_reads_rate_once() {
        let (profile, venue) = profile("0.2", Some("0.1"), false);
        profile
            .verify_margin_fee_admission(&account(), &[scope()], &caps("0.00003", "0.5"))
            .await
            .unwrap();
        venue.calls.lock().unwrap().clear();
        let error = profile
            .verify_margin_fee_admission(&account(), &[scope(), scope()], &caps("0.00003", "0.5"))
            .await
            .unwrap_err();
        assert_eq!(error.code, "KRAKEN_FEE_BOUND_EXCEEDED");
        assert_eq!(venue.calls.lock().unwrap().len(), 1);
        let evidence = profile
            .verify_margin_fee_admission(&account(), &[scope(), scope()], &caps("0.00004", "0.8"))
            .await
            .unwrap();
        assert_eq!(
            evidence["tradingEstimateByAsset"],
            json!({"XXBT":"0.00004","ZUSD":"0.8"})
        );
        assert_eq!(evidence["openingRate"]["creditLineFeesUsed"], false);
    }
    #[tokio::test]
    async fn base_and_quote_caps_are_independent_and_preferences_cannot_waive_either() {
        let (profile, _) = profile("0.2", None, false);
        for limits in [
            BTreeMap::from([("ZUSD".into(), "1".into())]),
            caps("0.000019", "1"),
            caps("1", "0.399"),
        ] {
            assert!(
                profile
                    .verify_margin_fee_admission(&account(), &[scope()], &limits)
                    .await
                    .is_err()
            );
        }
        let evidence = profile
            .verify_margin_fee_admission(&account(), &[scope()], &caps("0.00002", "0.4"))
            .await
            .unwrap();
        assert_eq!(evidence["markets"]["XXBTZUSD"]["admittedPercent"], "0.2");
    }
    #[tokio::test]
    async fn worst_positive_rate_is_used_and_rebates_never_reduce_reserved_estimates() {
        let (high, _) = profile("0.2", Some("0.4"), false);
        assert_eq!(
            high.verify_margin_fee_admission(&account(), &[scope()], &caps("0.00002", "0.4"))
                .await
                .unwrap_err()
                .code,
            "KRAKEN_FEE_BOUND_EXCEEDED"
        );
        let (rebate, _) = profile("0.2", Some("-0.1"), false);
        let evidence = rebate
            .verify_margin_fee_admission(&account(), &[scope()], &caps("0.00002", "0.4"))
            .await
            .unwrap();
        assert_eq!(evidence["tradingEstimateByAsset"]["ZUSD"], "0.4");
        let (missing, _) = profile("0.2", None, true);
        assert_eq!(
            missing
                .verify_margin_fee_admission(&account(), &[scope()], &caps("1", "1"))
                .await
                .unwrap_err()
                .code,
            "KRAKEN_FEE_PROOF_PENDING"
        );
    }
    #[tokio::test]
    async fn settlement_has_no_trading_fee_and_quote_observations_preserve_exact_units() {
        let (profile, venue) = profile("0.2", None, false);
        let mut settlement = scope();
        settlement.settling = true;
        let evidence = profile
            .verify_margin_fee_admission(&account(), &[settlement], &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(evidence["tradingEstimateByAsset"], json!({}));
        assert!(venue.calls.lock().unwrap().is_empty());
        let observed = profile
            .margin_fee_scope(scope(), &json!({"price":"21000","oflags":"fciq"}))
            .await
            .unwrap();
        assert_eq!(observed.quote_notional.to_decimal(36), "210");
        let current = profile
            .margin_fee_scope(scope(), &json!({"price":"+10"}))
            .await
            .unwrap();
        assert_eq!(current.quote_notional.to_decimal(36), "200");
    }
    #[tokio::test]
    async fn nonnative_quantity_units_or_invalid_rate_never_admit() {
        let (profile, _) = profile("not-a-percent", None, false);
        assert!(
            profile
                .verify_margin_fee_admission(&account(), &[scope()], &caps("1", "1"))
                .await
                .is_err()
        );
        let mut wrong = scope();
        wrong.base.decimals = 8;
        assert_eq!(
            profile
                .verify_margin_fee_admission(&account(), &[wrong], &caps("1", "1"))
                .await
                .unwrap_err()
                .code,
            "KRAKEN_FEE_ASSET_SCOPE_INVALID"
        );
    }
}

#[cfg(test)]
mod source_semantics_tests {
    use serde_json::Value;
    #[test]
    fn official_tier_minimum_and_volume_discount_currency_are_not_fixed_fees_or_margin_rates() {
        let raw: Value =
            serde_json::from_str(include_str!("../../../docs/kraken/spot-rest.json")).unwrap();
        let tier = &raw["components"]["schemas"]["fees"];
        assert!(
            tier["description"]
                .as_str()
                .unwrap()
                .contains("expressed as percentages")
        );
        assert!(
            tier["properties"]["minfee"]["description"]
                .as_str()
                .unwrap()
                .contains("highest volume tier")
        );
        let currency = raw["components"]["schemas"]
            .as_object()
            .unwrap()
            .values()
            .find_map(|schema| schema["properties"].get("fee_volume_currency"))
            .unwrap();
        assert_eq!(currency["description"], "Volume discount currency");
        assert!(tier["properties"].get("opening_fee").is_none());
        assert!(tier["properties"].get("rollover_fee").is_none());
    }
}
