//! Venue-specific observations. Cash, borrowed principal and contracts remain distinct.
use crate::{
    KrakenEndpoint,
    profile::{KrakenAccount, KrakenProduct, KrakenProfile, error},
};
use fin_domain::{
    Amount, AssetId, FinancialState, PositionKind, PositionState, Quantity, ResourceAmount,
    ResourceId, Result, StateSnapshot,
};
use num_traits::Signed;
use serde_json::{Value, json};

pub(crate) fn exact(value: &Value) -> Result<String> {
    let raw = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => {
            return Err(error(
                "KRAKEN_EVIDENCE_INVALID",
                "Kraken omitted an exact decimal quantity.",
            ));
        }
    };
    // Preserve JSON numeric lexemes without ever passing through f64.
    let mut parts = raw.split(['e', 'E']);
    let mantissa = parts.next().unwrap_or("");
    let exponent = parts
        .next()
        .map(str::parse::<i32>)
        .transpose()
        .map_err(|_| error("KRAKEN_EVIDENCE_INVALID", "Invalid exact decimal exponent."))?
        .unwrap_or(0);
    if parts.next().is_some() || exponent.unsigned_abs() > 100 || raw.len() > 256 {
        return Err(error(
            "KRAKEN_EVIDENCE_INVALID",
            "Kraken decimal exceeds the exact bounded representation.",
        ));
    }
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let mut decimal = unsigned.split('.');
    let integer = decimal.next().unwrap_or("");
    let fraction = decimal.next().unwrap_or("");
    if integer.is_empty()
        || decimal.next().is_some()
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(error(
            "KRAKEN_EVIDENCE_INVALID",
            "Invalid exact decimal lexeme.",
        ));
    }
    let digits = format!("{integer}{fraction}");
    let point = integer.len() as i32 + exponent;
    let normalized = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    if negative {
        Ok(format!("-{normalized}"))
    } else {
        Ok(normalized)
    }
}
pub(crate) fn atoms(value: &Value) -> Result<Quantity> {
    Quantity::from_decimal(&exact(value)?, 36)
}
/// BalanceEx omits both credit fields on ordinary cash accounts. Never infer
/// unused credit when a credit line is reported without its used amount.
pub(crate) fn spot_credit_used(row: &Value) -> Result<Quantity> {
    if let Some(credit) = row.get("credit") {
        atoms(credit)?;
        if row.get("credit_used").is_none() {
            return Err(error(
                "KRAKEN_EVIDENCE_INVALID",
                "Kraken reported a credit line without its used amount.",
            ));
        }
    }
    row.get("credit_used")
        .map(atoms)
        .transpose()
        .map(|used| used.unwrap_or_default())
}
pub(crate) fn asset(code: &str, product: KrakenProduct) -> AssetId {
    AssetId {
        network: product.client().into(),
        reference: code.into(),
        decimals: 36,
    }
}
pub(crate) fn result(value: &Value) -> &Value {
    value
        .get("result")
        .filter(|v| v.is_object())
        .unwrap_or(value)
}
impl KrakenProfile {
    pub(crate) async fn spot_asset(&self, _account: &KrakenAccount, code: &str) -> Result<AssetId> {
        let metadata = self
            .json(
                KrakenEndpoint::KrakenPublicGetAssets,
                None,
                json!({"asset":code}),
            )
            .await?;
        let candidates = result(&metadata)
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_ASSET_UNKNOWN",
                    "Kraken asset metadata is not an identity map.",
                )
            })?
            .iter()
            .filter(|(key, record)| key.as_str() == code || record["altname"] == code)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(error(
                "KRAKEN_ASSET_UNKNOWN",
                "Kraken did not return one exact native or venue-verified asset alias identity.",
            ));
        }
        let (canonical, record) = candidates[0];
        if record
            .get("aclass")
            .is_some_and(|class| class != "currency")
        {
            return Err(error(
                "KRAKEN_ASSET_CLASS_UNSUPPORTED",
                "This workflow requires an exact currency identity; use an explicitly reviewed asset-class operation for tokenized assets.",
            ));
        }
        if record["decimals"].as_u64().is_none_or(|n| n > 36) {
            return Err(error(
                "KRAKEN_ASSET_PRECISION_UNKNOWN",
                "Kraken asset precision is unavailable or exceeds exact accounting precision.",
            ));
        }
        Ok(asset(canonical, KrakenProduct::Spot))
    }
    pub(crate) async fn check_native_custody_aliases(&self) -> Result<Vec<String>> {
        let mut coverage = vec![];
        let mut native_identities = std::collections::BTreeSet::new();
        // Check enrolled credential aliases before observing any custody. Wallet
        // scopes below are intentional distinct native subaccounts of one key.
        for account in &self.accounts {
            let response = self
                .json(
                    if self.product == KrakenProduct::Spot {
                        KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo
                    } else {
                        KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check
                    },
                    Some(account),
                    json!({}),
                )
                .await?;
            let identity = if self.product == KrakenProduct::Spot {
                result(&response)
                    .get("iban")
                    .or_else(|| result(&response).get("iiban"))
            } else {
                response.get("accountUid")
            };
            if let Some(identity) = identity
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                if !native_identities.insert(crate::contracts::canonical_account_identity(identity))
                {
                    return Err(error(
                        "KRAKEN_DUPLICATE_CUSTODY_ALIAS",
                        "Enrolled credential aliases refer to the same authenticated native custody account. Select one protected connection before observing or executing.",
                    ));
                }
                coverage.push(format!(
                    "{}:credential account identity verified by venue metadata",
                    account.name
                ));
            } else {
                coverage.push(format!("{}:venue omitted native account identity; custody alias relationship remains explicitly human-verified",account.name));
            }
        }
        Ok(coverage)
    }
    pub(crate) async fn observe_financial(&self) -> Result<StateSnapshot> {
        let mut state = FinancialState::default();
        let mut coverage = vec![];
        let mut pending = vec![];
        coverage.extend(self.check_native_custody_aliases().await?);
        for account in &self.accounts {
            match self.product {
                KrakenProduct::Spot => {
                    let metadata = self
                        .json(KrakenEndpoint::KrakenPublicGetAssets, None, json!({}))
                        .await?;
                    let scopes = self.account_scopes(account).await?;
                    let scoped_balance =
                        crate::contracts::contract(KrakenEndpoint::KrakenPrivatePostBalance)
                            .schema
                            .as_ref()
                            .and_then(|s| s["properties"].get("account_id"))
                            .is_some();
                    if !scoped_balance && scopes.len() > 1 {
                        pending.push(format!("{}:additional wallet custody cannot be observed with the pinned unscoped Balance API",account.name));
                    }
                    let default_scopes = vec![account.clone()];
                    for scope in if scoped_balance {
                        &scopes
                    } else {
                        &default_scopes
                    } {
                        let balances = self
                            .json(
                                KrakenEndpoint::KrakenPrivatePostBalance,
                                Some(scope),
                                json!({"rebase_multiplier":"rebased"}),
                            )
                            .await?;
                        for (code, balance) in result(&balances).as_object().ok_or_else(|| {
                            error(
                                "KRAKEN_EVIDENCE_INVALID",
                                "Spot balance response is not an exact asset map.",
                            )
                        })? {
                            let q = atoms(balance)?;
                            if q.is_zero() {
                                continue;
                            }
                            // Earn suffix balances represent the same custody as allocations,
                            // not spendable spot cash. Legacy staking variants remain disclosed.
                            if [".B", ".F", ".S", ".M"]
                                .iter()
                                .any(|suffix| code.ends_with(suffix))
                            {
                                if code.ends_with(".S") || code.ends_with(".M") {
                                    pending.push(format!(
                                        "{}:{code}:legacy staking position coverage pending",
                                        account.name
                                    ));
                                }
                                continue;
                            }
                            if code.ends_with(".T") {
                                let tokenized = self
                                    .json(
                                        KrakenEndpoint::KrakenPublicGetAssets,
                                        None,
                                        json!({"asset":code,"aclass":"tokenized_asset"}),
                                    )
                                    .await?;
                                let record = &result(&tokenized)[code];
                                if record["aclass"] != "tokenized_asset"
                                    || record["decimals"].as_u64().is_none_or(|n| n > 36)
                                {
                                    pending.push(format!(
                                        "{}:{code}:tokenized rebased asset metadata is unavailable",
                                        scope.name
                                    ));
                                    continue;
                                }
                                state.holdings.push(ResourceAmount {
                                    resource: scope.resource.clone(),
                                    amount: Amount {
                                        asset: AssetId {
                                            network: "kraken:tokenized_asset:rebased".into(),
                                            reference: code.clone(),
                                            decimals: 36,
                                        },
                                        atoms: q,
                                    },
                                });
                                continue;
                            }
                            if result(&metadata).get(code).is_none() {
                                pending.push(format!(
                                    "{}:{code}:asset identity unavailable",
                                    account.name
                                ));
                                continue;
                            }
                            state.holdings.push(ResourceAmount {
                                resource: scope.resource.clone(),
                                amount: Amount {
                                    asset: asset(code, self.product),
                                    atoms: q,
                                },
                            });
                        }
                    }
                    if account.default_wallet_account_id.is_none() {
                        coverage.push(format!("{}:bounded enrolled default wallet; additional wallet-account custody is unobserved until a protected default-wallet relationship is configured",account.name));
                    }
                    state
                        .positions
                        .extend(self.margin_snapshot_positions(account).await?);
                    state.positions.extend(self.observe_earn(account).await?);
                    let credit = self
                        .json(
                            KrakenEndpoint::KrakenPrivatePostCreditLines,
                            Some(account),
                            json!({}),
                        )
                        .await?;
                    if result(&credit).as_object().is_none_or(|c| !c.is_empty()) {
                        pending.push(format!("{}:active credit lines require separately verified liability principal",account.name));
                    }
                    coverage.push(format!("{}:exact returned spot custody assets and Earn strategies; margin and credit liabilities declared when present",account.name));
                }
                KrakenProduct::Derivatives => {
                    let accounts = self
                        .json(
                            KrakenEndpoint::KrakenfuturesPrivateGetAccounts,
                            Some(account),
                            json!({}),
                        )
                        .await?;
                    for (wallet, record) in accounts["accounts"].as_object().ok_or_else(|| {
                        error(
                            "KRAKEN_EVIDENCE_INVALID",
                            "Derivatives wallets response is missing its account map.",
                        )
                    })? {
                        let resource =
                            ResourceId(format!("{}:wallet:{wallet}", account.resource.0));
                        if record["balances"].is_object() && record["currencies"].is_object() {
                            return Err(error(
                                "KRAKEN_COLLATERAL_CUSTODY_DUPLICATE",
                                "A native wallet returned both cash-balance and multicollateral-currency views; Fin cannot count overlapping custody or infer their relationship.",
                            ));
                        }
                        if let Some(balances) = record["balances"].as_object() {
                            for (code, value) in balances {
                                // Contract balances are exposure, never custody assets.
                                if code.starts_with("PI_")
                                    || code.starts_with("PF_")
                                    || code.starts_with("FI_")
                                {
                                    continue;
                                }
                                let q = atoms(value)?;
                                if !q.is_zero() {
                                    state.holdings.push(ResourceAmount {
                                        resource: resource.clone(),
                                        amount: Amount {
                                            asset: asset(code, self.product),
                                            atoms: q,
                                        },
                                    });
                                }
                            }
                        }
                        if let Some(currencies) = record["currencies"].as_object() {
                            for (code, value) in currencies {
                                let uid = if wallet == "flex"
                                    && code == "USD"
                                    && crate::derivative_fee_units::signed_native(
                                        &value["quantity"],
                                    )?
                                    .is_negative()
                                {
                                    self.json(KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check,Some(account),json!({})).await?["accountUid"].as_str().filter(|uid|!uid.is_empty()).ok_or_else(||error("KRAKEN_ACCOUNT_IDENTITY_UNPROVEN","A native USD currency-deficit snapshot requires authenticated account UID evidence."))?.to_owned()
                                } else {
                                    String::new()
                                };
                                let native = crate::derivative_fee_units::observe_currency(
                                    &resource,
                                    wallet,
                                    code,
                                    &value["quantity"],
                                    &uid,
                                )?;
                                state.holdings.extend(native.holding);
                                state.positions.extend(native.liability);
                                pending.extend(native.pending);
                            }
                        }
                    }
                    let positions = self
                        .json(
                            KrakenEndpoint::KrakenfuturesPrivateGetOpenpositions,
                            Some(account),
                            json!({}),
                        )
                        .await?;
                    for p in positions["openPositions"].as_array().ok_or_else(|| {
                        error(
                            "KRAKEN_EVIDENCE_INVALID",
                            "Derivative positions response is missing its position list.",
                        )
                    })? {
                        let symbol = p["symbol"].as_str().ok_or_else(|| {
                            error("KRAKEN_EVIDENCE_INVALID", "Derivative symbol missing.")
                        })?;
                        let kind = match p["side"].as_str() {
                            Some("long") => PositionKind::DerivativeLong,
                            Some("short") => PositionKind::DerivativeShort,
                            _ => {
                                return Err(error(
                                    "KRAKEN_EVIDENCE_INVALID",
                                    "Derivative side missing.",
                                ));
                            }
                        };
                        state.positions.push(PositionState {
                            resource: account.resource.clone(),
                            position: symbol.into(),
                            kind,
                            amount: Amount {
                                asset: AssetId {
                                    network: "krakenfutures:contracts".into(),
                                    reference: symbol.into(),
                                    decimals: 36,
                                },
                                atoms: atoms(&p["size"])?,
                            },
                            scaled_receipt: None,
                            anchor: None,
                        });
                    }
                    coverage.push(format!("{}:returned derivative collateral wallets and contract exposures; unrealized PnL excluded",account.name));
                }
            }
        }
        Ok(StateSnapshot {
            state,
            observed_at: fin_domain::now(),
            complete: pending.is_empty(),
            coverage,
            pending,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_number_lexemes_never_round() {
        assert_eq!(
            atoms(&serde_json::from_str("0.123456789012345678901234567890").unwrap())
                .unwrap()
                .to_decimal(36),
            "0.12345678901234567890123456789"
        );
        assert_eq!(
            exact(&serde_json::from_str("1.234e-8").unwrap()).unwrap(),
            "0.00000001234"
        );
        assert!(atoms(&json!("-1")).is_err());
    }
}

#[cfg(test)]
mod alias_tests {
    use super::*;
    use crate::{KrakenResponse, KrakenTransport};
    use async_trait::async_trait;
    use fin_exchange::kraken::DispatchGuard;
    use std::sync::{Arc, Mutex};
    struct IdentityOnlyWire {
        calls: Mutex<Vec<KrakenEndpoint>>,
    }
    #[async_trait]
    impl KrakenTransport for IdentityOnlyWire {
        async fn request(
            &self,
            e: KrakenEndpoint,
            _: Option<&KrakenAccount>,
            _: Value,
            g: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(g.is_none());
            self.calls.lock().unwrap().push(e);
            Ok(KrakenResponse::Json(match e {
                KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo => {
                    json!({"result":{"iban":"AA11BB22CC33DD44","permissions":["query-funds"]}})
                }
                KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check => {
                    json!({"accountUid":"11111111-2222-3333-4444-555555555555","permissions":{"general":"READ_ONLY"}})
                }
                _ => {
                    panic!("Custody must never be read before duplicate-native-identity protection")
                }
            }))
        }
    }
    fn account(name: &str, id: &str, product: KrakenProduct) -> KrakenAccount {
        KrakenAccount {
            name: name.into(),
            resource: ResourceId(format!("synthetic:{name}")),
            credential_ref: format!("synthetic-key:{name}"),
            physical_identity: id.into(),
            identity_verified: true,
            wallet_account_id: None,
            default_wallet_account_id: None,
            product,
        }
    }
    #[test]
    fn differently_formatted_protected_identity_aliases_are_rejected_before_custody() {
        for (product, first, second) in [
            (
                KrakenProduct::Spot,
                "AA11BB22CC33DD44",
                "aa11 bb22 cc33 dd44",
            ),
            (
                KrakenProduct::Derivatives,
                "11111111-2222-3333-4444-555555555555",
                "11111111222233334444555555555555",
            ),
        ] {
            let wire = Arc::new(IdentityOnlyWire {
                calls: Mutex::new(vec![]),
            });
            let error = KrakenProfile::new(
                product,
                vec![
                    account("one", first, product),
                    account("two", second, product),
                ],
                wire.clone(),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, "KRAKEN_ACCOUNT_INVALID");
            assert!(wire.calls.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn authenticated_alias_guard_rejects_distinct_configured_uid_iiban_aliases_before_holdings()
     {
        for product in [KrakenProduct::Spot, KrakenProduct::Derivatives] {
            let wire = Arc::new(IdentityOnlyWire {
                calls: Mutex::new(vec![]),
            });
            let profile = KrakenProfile::new(
                product,
                vec![
                    account("one", "11111111-2222-3333-4444-555555555555", product),
                    account("two", "AA11BB22CC33DD44", product),
                ],
                wire.clone(),
            )
            .unwrap();
            assert_eq!(
                profile.observe_financial().await.unwrap_err().code,
                "KRAKEN_DUPLICATE_CUSTODY_ALIAS"
            );
            assert_eq!(wire.calls.lock().unwrap().len(), 2);
        }
    }
    #[tokio::test]
    async fn authenticated_alias_guard_runs_before_financial_preview_without_balances_or_dispatch()
    {
        use fin_exchange::ExchangeProfile;
        for product in [KrakenProduct::Spot, KrakenProduct::Derivatives] {
            let wire = Arc::new(IdentityOnlyWire {
                calls: Mutex::new(vec![]),
            });
            let profile = KrakenProfile::new(
                product,
                vec![
                    account("one", "11111111-2222-3333-4444-555555555555", product),
                    account("two", "AA11BB22CC33DD44", product),
                ],
                wire.clone(),
            )
            .unwrap();
            let operation = if product == KrakenProduct::Spot {
                KrakenEndpoint::KrakenPrivatePostAddOrder
            } else {
                KrakenEndpoint::KrakenfuturesPrivatePostSendorder
            };
            assert_eq!(
                profile
                    .prepare(operation.operation(), &json!({}))
                    .await
                    .unwrap_err()
                    .code,
                "KRAKEN_DUPLICATE_CUSTODY_ALIAS"
            );
            assert_eq!(wire.calls.lock().unwrap().len(), 2);
        }
    }
}
