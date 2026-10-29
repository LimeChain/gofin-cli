use super::*;
use gofin_domain::clock::Clock as _;
use gofin_pricing::{UsdPriceSource, UsdQuote};
use std::sync::atomic::AtomicI64;

struct Clock(AtomicI64);
impl gofin_domain::clock::Clock for Clock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
    }
}

struct BatchPrices {
    requests: Mutex<Vec<Vec<AssetId>>>,
    clock: Arc<Clock>,
}
#[async_trait]
impl UsdPriceSource for BatchPrices {
    async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
        panic!("policy valuation must fetch source and gas prices in one batch");
    }
    async fn quotes(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
        self.requests.lock().unwrap().push(assets.to_vec());
        // Model an already cached gas row and one paced source-price fetch.
        // Retrieval cache expiry must not force a second, sequential gas fetch.
        let cached_at = self.clock.now().to_rfc3339();
        self.clock.0.fetch_add(48, Ordering::SeqCst);
        Ok(assets
            .iter()
            .map(|asset| UsdQuote {
                usd_per_unit: if asset.reference == "native" {
                    "2000.123456789"
                } else {
                    "1.012345"
                }
                .into(),
                updated_at: if asset.reference == "native" {
                    cached_at.clone()
                } else {
                    self.clock.now().to_rfc3339()
                },
                observed_at: self.clock.now().to_rfc3339(),
                source: "synthetic batched USD quote".into(),
            })
            .collect())
    }
}

fn assets() -> [AssetId; 2] {
    [
        AssetId {
            network: "ethereum:1".into(),
            reference: ETHEREUM_USDC.into(),
            decimals: 6,
        },
        AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        },
    ]
}

#[tokio::test]
async fn bridge_policy_valuation_batches_source_and_gas_once_and_rounds_each_amount() {
    let clock = Arc::new(Clock(AtomicI64::new(1_800_000_000)));
    let source = Arc::new(BatchPrices {
        requests: Default::default(),
        clock: clock.clone(),
    });
    let quote_calls = Arc::new(AtomicUsize::new(0));
    let adapter = BridgeAdapter::with_providers(
        vec![Arc::new(ScopeOnlyProvider {
            provider: "relay",
            quote_calls: quote_calls.clone(),
        })],
        BTreeMap::new(),
        BTreeMap::new(),
        Arc::new(SharedUsdValuator::with_source(source.clone()).with_clock(clock.clone())),
    )
    .unwrap()
    .with_clock(clock.clone());
    let [token, native] = assets();
    let resource = ResourceId("ethereum:1:synthetic".into());
    let amount = |asset: &AssetId, atoms: &str| Amount {
        asset: asset.clone(),
        atoms: Quantity::new(atoms).unwrap(),
    };
    let intent = FinancialIntent {
        debits: vec![
            ResourceAmount {
                resource: resource.clone(),
                amount: amount(&token, "1"),
            },
            ResourceAmount {
                resource: resource.clone(),
                amount: amount(&token, "1"),
            },
        ],
        maximum_costs: vec![
            CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: amount(&native, "1000000000"),
                treatment: CostTreatment::AdditionalDebit,
            },
            CostBound {
                location: AccountingLocation::Cash { resource },
                amount: amount(&native, "1000000000"),
                treatment: CostTreatment::AdditionalDebit,
            },
        ],
        ..FinancialIntent::default()
    };
    let valuation = adapter.value_intent(&intent).await.unwrap();
    assert_eq!(*source.requests.lock().unwrap(), vec![vec![token, native]]);
    assert_eq!(
        valuation.movement_usd_micros.as_str(),
        "4",
        "ceil applies to each debit before summing"
    );
    assert_eq!(
        valuation.maximum_fee_usd_micros.as_str(),
        "6",
        "ceil applies to each gas cap before summing"
    );
    assert_eq!(valuation.prices.len(), 2);
    assert_eq!(
        valuation.prices[1].usd_micros_per_unit.as_str(),
        "2000123457"
    );
    assert_eq!(
        valuation.observed_at,
        chrono::DateTime::from_timestamp(1_800_000_048, 0)
            .unwrap()
            .to_rfc3339()
    );
    assert_eq!(
        quote_calls.load(Ordering::SeqCst),
        0,
        "valuation never asks a bridge provider to quote or write"
    );
}

#[test]
fn bridge_price_batch_rejects_incomplete_duplicate_mismatched_or_invalid_rows() {
    let assets = assets();
    let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let valid = assets
        .iter()
        .map(|asset| AssetPrice {
            asset: asset.clone(),
            usd_micros_per_unit: Quantity::from_u64(1_000_000),
            unit_atoms: Quantity::from_decimal("1", asset.decimals).unwrap(),
            observed_at: now.to_rfc3339(),
            source: "synthetic batch".into(),
        })
        .collect::<Vec<_>>();
    assert!(BridgeAdapter::validated_prices(&assets, valid.clone(), now).is_ok());
    let mut invalid_sets = vec![
        vec![],
        vec![valid[0].clone()],
        vec![valid[0].clone(); 2],
        vec![valid[0].clone(); 3],
    ];
    let mut reversed = valid.clone();
    reversed.reverse();
    invalid_sets.push(reversed);
    for change in 0..8 {
        let mut rows = valid.clone();
        match change {
            0 => rows[0].asset.decimals = 18,
            1 => rows[0].unit_atoms = Quantity::from_u64(1),
            2 => rows[0].usd_micros_per_unit = Quantity::zero(),
            3 => rows[0].observed_at = (now - chrono::Duration::seconds(120)).to_rfc3339(),
            4 => rows[0].observed_at = (now + chrono::Duration::seconds(1)).to_rfc3339(),
            5 => rows[0].observed_at = "invalid".into(),
            6 => rows[0].source = String::new(),
            7 => rows[0].source = "a".repeat(1025),
            _ => unreachable!(),
        }
        invalid_sets.push(rows);
    }
    for rows in invalid_sets {
        assert_eq!(
            BridgeAdapter::validated_prices(&assets, rows, now)
                .unwrap_err()
                .code,
            "BRIDGE_VALUATION_INVALID"
        );
    }
    assert!(
        BridgeAdapter::validated_prices(
            &vec![assets[0].clone(); 2],
            vec![valid[0].clone(); 2],
            now
        )
        .is_err()
    );
    let mut invalid_asset = assets[0].clone();
    invalid_asset.decimals = 37;
    let mut row = valid[0].clone();
    row.asset = invalid_asset.clone();
    assert!(BridgeAdapter::validated_prices(&[invalid_asset], vec![row], now).is_err());
}

struct CompositePrices {
    now: chrono::DateTime<chrono::Utc>,
    principal_age: i64,
}
#[async_trait]
impl UsdPriceSource for CompositePrices {
    async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
        panic!("batch required")
    }
    async fn quotes(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
        Ok(assets.iter().map(|asset| {
            let updated = self.now - chrono::Duration::seconds(if asset.reference == "native" {10} else {self.principal_age});
            let retrieved = self.now - chrono::Duration::seconds(1);
            UsdQuote { usd_per_unit: "1".into(), updated_at: updated.to_rfc3339(), observed_at: retrieved.to_rfc3339(), source: if asset.reference == "native" {"independent native price".into()} else {format!("Stablecoin/USD conversion; exchange=coinbase; exact_asset=USDC; provider; updated:{}; conversion retrieval; updated:{}",updated.to_rfc3339(),retrieved.to_rfc3339())} }
        }).collect())
    }
}

#[tokio::test]
async fn aggregate_calculation_keeps_independent_leg_timestamps_and_rejects_stale_principal() {
    for principal_age in [150, 300, -1] {
        let clock = Arc::new(Clock(AtomicI64::new(1_800_000_000)));
        let source = Arc::new(CompositePrices {
            now: clock.now(),
            principal_age,
        });
        let adapter = BridgeAdapter::with_providers(
            vec![Arc::new(ScopeOnlyProvider {
                provider: "relay",
                quote_calls: Arc::new(AtomicUsize::new(0)),
            })],
            BTreeMap::new(),
            BTreeMap::new(),
            Arc::new(SharedUsdValuator::with_source(source).with_clock(clock.clone())),
        )
        .unwrap()
        .with_clock(clock.clone());
        let [token, native] = assets();
        let resource = ResourceId("ethereum:1:synthetic".into());
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: Quantity::from_u64(1_000_000),
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset: native,
                    atoms: Quantity::from_u64(1),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            ..Default::default()
        };
        let result = adapter.value_intent(&intent).await;
        if principal_age != 150 {
            assert!(
                result.is_err(),
                "stale/future principal cannot be hidden behind a fresh fee"
            );
            continue;
        }
        let valuation = result.unwrap();
        assert_eq!(valuation.observed_at, clock.now().to_rfc3339());
        assert!(gofin_domain::price_freshness::is_fresh(
            &valuation.observed_at,
            &valuation.source,
            clock.now()
        ));
        let row = valuation
            .prices
            .iter()
            .find(|row| row.asset == token)
            .unwrap();
        assert_eq!(
            row.observed_at,
            (clock.now() - chrono::Duration::seconds(150)).to_rfc3339()
        );
        assert!(row.source.contains("conversion retrieval"));
        assert!(
            valuation
                .prices
                .iter()
                .all(|row| gofin_domain::price_freshness::is_fresh(
                    &row.observed_at,
                    &row.source,
                    clock.now()
                ))
        );
        assert!(
            adapter
                .value_intent(&FinancialIntent::default())
                .await
                .is_err()
        );
    }
}
