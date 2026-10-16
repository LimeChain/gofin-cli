//! Native funding/Earn cost proofs through the actual adapter, Engine and SQLite.
//! All identities, fees, credentials and responses below are synthetic.
use super::super::*;
use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
use gofin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
#[derive(Clone, Copy, Debug)]
enum Family {
    Claim,
    Allocate,
    Joint,
    Deallocate,
    Legacy,
    Current,
    Separate,
    Failed,
}
struct Venue {
    family: Family,
    submitted: AtomicBool,
    writes: AtomicUsize,
    ambiguous: AtomicBool,
    bounded_fees: AtomicBool,
}
fn address() -> Value {
    json!({"crypto":{"address":"0x1111111111111111111111111111111111111111"}})
}
fn method() -> &'static str {
    "00000000-0000-0000-0000-000000000001"
}
fn recipient() -> &'static str {
    "ABabcde-abcde-abcdef"
}
fn account() -> KrakenAccount {
    KrakenAccount {
        name: "SYNTHETIC-funding".into(),
        resource: ResourceId("kraken:SYNTHETIC-funding".into()),
        credential_ref: "SYNTHETIC-NOT-A-KEY".into(),
        physical_identity: "SYNTHETIC-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    }
}
impl Venue {
    fn new(family: Family) -> Arc<Self> {
        Arc::new(Self {
            family,
            submitted: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            ambiguous: AtomicBool::new(false),
            bounded_fees: AtomicBool::new(false),
        })
    }
    fn active(&self) -> bool {
        self.submitted.load(Ordering::SeqCst)
    }
    fn allocations(&self) -> Value {
        let total = if !self.active() {
            "1"
        } else if self.bounded_fees.load(Ordering::SeqCst)
            && matches!(self.family, Family::Allocate)
        {
            "1.09975"
        } else if self.bounded_fees.load(Ordering::SeqCst)
            && matches!(self.family, Family::Deallocate)
        {
            "0.89975"
        } else if matches!(self.family, Family::Joint) {
            "1.09925"
        } else if matches!(self.family, Family::Deallocate) {
            "0.898"
        } else {
            "1.1"
        };
        json!({"result":{"items":[{"strategy_id":"ES-SYNTHETIC","native_asset":"XETH","amount_allocated":{"total":{"native":total}}}]}})
    }
    fn ledger(&self) -> Value {
        if !self.active() {
            return json!({"result":{"ledger":{}}});
        }
        let rows = match self.family {
            Family::Claim => {
                json!({"SYNTHETIC-setup":{"refid":"SYNTHETIC-new-address","type":"spend","asset":"XETH","amount":"-0.002","fee":"0"}})
            }
            Family::Joint => {
                json!({"SYNTHETIC-cash":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH","amount":"-0.1","fee":"0.00075"},"SYNTHETIC-position":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH.B","amount":"0.1","fee":"0.00075"}})
            }
            Family::Allocate => {
                json!({"SYNTHETIC-cash":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH","amount":"-0.1","fee":"0.002"},"SYNTHETIC-position":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH.B","amount":"0.1","fee":"0"}})
            }
            Family::Deallocate => {
                json!({"SYNTHETIC-cash":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH","amount":"0.1","fee":"0"},"SYNTHETIC-position":{"refid":"SYNTHETIC-earn-reference","type":"transfer","asset":"XETH.B","amount":"-0.1","fee":"0.002"}})
            }
            Family::Separate => {
                json!({"SYNTHETIC-withdraw":{"refid":"SYNTHETIC-withdrawal","type":"withdrawal","asset":"XETH","amount":"-0.1","fee":"0"},"SYNTHETIC-fee":{"refid":"SYNTHETIC-withdrawal","type":"withdrawal","asset":"USDC","amount":"-0.002","fee":"0"}})
            }
            Family::Failed => {
                json!({"SYNTHETIC-withdraw":{"refid":"SYNTHETIC-withdrawal","type":"withdrawal","asset":"XETH","amount":"-0.1","fee":"0.002"},"SYNTHETIC-return":{"refid":"SYNTHETIC-withdrawal","type":"deposit","asset":"XETH","amount":"0.1","fee":"0"}})
            }
            _ => {
                json!({"SYNTHETIC-withdraw":{"refid":"SYNTHETIC-withdrawal","type":"withdrawal","asset":"XETH","amount":"-0.1","fee":"0.002"}})
            }
        };
        let mut rows = rows;
        if self.bounded_fees.load(Ordering::SeqCst)
            && matches!(self.family, Family::Allocate | Family::Deallocate)
        {
            for row in rows.as_object_mut().unwrap().values_mut() {
                row["fee"] = json!("0.00025");
            }
        }
        if self.ambiguous.load(Ordering::SeqCst) {
            let (id, row) = rows.as_object().unwrap().iter().next().unwrap();
            let id = format!("{id}-duplicate");
            let row = row.clone();
            rows[id] = row;
        }
        json!({"result":{"ledger":rows}})
    }
}
#[async_trait]
impl KrakenTransport for Venue {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e.mutating() {
            assert!(guard.expect("protected dispatch guard")());
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.submitted.store(true, Ordering::SeqCst);
        }
        let fee_asset = if matches!(self.family, Family::Separate) {
            "USDC"
        } else {
            "XETH"
        };
        let gross = if matches!(self.family, Family::Separate) {
            "0.1"
        } else {
            "0.101"
        };
        let data = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-ledger","earn-funds","withdraw-funds","add-funds"]}})
            }
            E::KrakenPublicGetAssets => {
                json!({"result":{"XETH":{"decimals":18},"USDC":{"decimals":6}}})
            }
            E::KrakenPrivatePostBalance => {
                json!({"result":{"XETH":if matches!(self.family,Family::Deallocate){"0"}else{"10"},"USDC":"10"}})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"result":{"XETH":{"balance":"10","hold_trade":"0"},"USDC":{"balance":"10","hold_trade":"0"}}})
            }
            E::KrakenPrivatePostOpenPositions => json!({"result":{}}),
            E::KrakenPrivatePostCreditLines => json!({"result":{}}),
            E::KrakenPrivatePostLedgers => {
                let mut response = self.ledger();
                if let Some(asset) = parameters["asset"].as_str().filter(|asset| *asset != "all") {
                    response["result"]["ledger"]
                        .as_object_mut()
                        .unwrap()
                        .retain(|_, row| row["asset"] == asset);
                }
                response
            }
            E::KrakenPrivatePostEarnStrategies => {
                json!({"result":{"items":[{"id":"ES-SYNTHETIC","asset":"XETH","can_allocate":true,"can_deallocate":true,"allocation_fee":"0.001","deallocation_fee":"0.001","lock_type":{"type":"instant"}}],"next_cursor":null}})
            }
            E::KrakenPrivatePostEarnAllocations => self.allocations(),
            E::KrakenPrivatePostEarnAllocateStatus | E::KrakenPrivatePostEarnDeallocateStatus => {
                json!({"result":{"pending":false}})
            }
            E::KrakenPrivatePostEarnAllocate | E::KrakenPrivatePostEarnDeallocate => {
                json!({"result":true})
            }
            E::KrakenFundingGetFundingV1MethodsDirection => {
                json!({"methods":[{"method_id":method(),"method_name":"Ether (Hex)","asset":{"class":"currency","name":"XETH"},"network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":"Ethereum"},"minimum_amount":"0.01","maximum_amount":"2","deposit":{"address_generation":{"status":"unlimited"},"address_setup_fee":{"asset":{"class":"currency","name":"XETH"},"amount":"0.001"}},"fees":{"base":{"asset":{"class":"currency","name":fee_asset},"amount":"0.001"},"included":!matches!(self.family,Family::Separate)}}]})
            }
            E::KrakenFundingGetFundingV2DepositAddresses => {
                json!({"addresses":if self.active(){vec![json!({"id":"SYNTHETIC-new-address","method_id":method(),"address_details":address()})]}else{vec![]}})
            }
            E::KrakenFundingPutFundingV1DepositAddress => json!({"address_details":address()}),
            E::KrakenPrivatePostWithdrawInfo => {
                json!({"result":{"method":"Ether (Hex)","amount":"0.1","fee":"0.001","limit":"1"}})
            }
            E::KrakenPrivatePostWithdrawAddresses => {
                json!({"result":[{"key":"SYNTHETIC-destination","method":"Ether (Hex)","address":address()["crypto"]["address"],"verified":true}]})
            }
            E::KrakenPrivatePostWithdrawMethods => {
                json!({"result":[{"method":"Ether (Hex)","network":"Ethereum"}]})
            }
            E::KrakenPrivatePostWithdraw => json!({"result":{"refid":"SYNTHETIC-withdrawal"}}),
            E::KrakenPrivatePostWithdrawStatus => {
                json!({"result":[{"refid":"SYNTHETIC-withdrawal","status":if matches!(self.family,Family::Failed){"Failure"}else{"Success"},"asset":"XETH","method":"Ether (Hex)","key":"SYNTHETIC-destination","info":address()["crypto"]["address"],"amount":"0.1","fee":"0.002"}]})
            }
            E::KrakenFundingGetFundingV1Addresses => {
                json!({"addresses":[{"address_id":recipient(),"scope":{"method_id":method()},"verified":true,"address_details":address()}]})
            }
            E::KrakenFundingGetFundingV1FeesMethodId => {
                json!({"fee":{"asset":{"class":"currency","name":fee_asset},"amount":"0.001"},"gross_amount":{"asset":{"class":"currency","name":"XETH"},"amount":gross},"net_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"}})
            }
            E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset => {
                json!({"withdrawal_limits":[{"method_id":method(),"maximum_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"1"}}]})
            }
            E::KrakenFundingPostFundingV1Withdrawals => {
                json!({"withdrawal_id":"SYNTHETIC-withdrawal","gross_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":gross}},"net_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"}},"fee":{"asset_amount":{"asset":{"class":"currency","name":fee_asset},"amount":"0.001"}}})
            }
            E::KrakenFundingGetFundingV1Withdrawals => {
                json!({"withdrawals":[{"withdrawal_id":"SYNTHETIC-withdrawal","address_id":recipient(),"method_id":method(),"amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"},"fee":{"asset":{"class":"currency","name":fee_asset},"amount":"0.002"},"status":"success"}]})
            }
            _ => panic!("Unexpected synthetic native funding request {e:?}"),
        };
        Ok(KrakenResponse::Json(data))
    }
}
struct Quotes;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Quotes {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: "2000".into(),
            source: format!("SYNTHETIC-exact-{}", asset.reference),
            updated_at: now(),
            observed_at: now(),
        })
    }
}
struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "SYNTHETIC-auto-test-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn engine(path: &std::path::Path, venue: Arc<Venue>) -> gofin_engine::Engine {
    let profile = KrakenProfile::new(KrakenProduct::Spot, vec![account()], venue)
        .unwrap()
        .with_prices(Arc::new(Quotes));
    gofin_engine::Engine::new(
        gofin_store::Store::open(path).unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile))],
        Arc::new(Approval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
fn request(family: Family) -> OperationRequest {
    let (endpoint, input) = match family {
        Family::Claim => (
            E::KrakenFundingPutFundingV1DepositAddress,
            json!({"method_id":method(),"maxFee":"0.001"}),
        ),
        Family::Allocate | Family::Joint | Family::Deallocate => (
            if matches!(family, Family::Allocate | Family::Joint) {
                E::KrakenPrivatePostEarnAllocate
            } else {
                E::KrakenPrivatePostEarnDeallocate
            },
            json!({"strategy_id":"ES-SYNTHETIC","amount":"0.1","maxFee":"0.001"}),
        ),
        Family::Legacy | Family::Failed => (
            E::KrakenPrivatePostWithdraw,
            json!({"asset":"XETH","key":"SYNTHETIC-destination","amount":"0.101","max_fee":"0.001","verificationRequirement":"venue_only"}),
        ),
        _ => (
            E::KrakenFundingPostFundingV1Withdrawals,
            json!({"scope":{"method_id":method()},"address_id":recipient(),"amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":if matches!(family,Family::Separate){"0.1"}else{"0.101"}}},"fee":{"fee_included":!matches!(family,Family::Separate)},"maxFee":"0.001","verificationRequirement":"venue_only"}),
        ),
    };
    let mut input = input;
    if matches!(family, Family::Current | Family::Separate) {
        input["fee"]["current_fee"] = json!({});
    }

    input["account"] = json!(account().name);
    input["requestId"] = json!("SYNTHETIC-funding-overrun");
    OperationRequest {
        request_id: "SYNTHETIC-funding-overrun".into(),
        operation: endpoint.operation().into(),
        input,
        run_id: None,
    }
}
#[tokio::test]
async fn funding_setup_earn_and_withdrawal_overruns_persist_exact_cost_without_replay_or_authority_expansion()
 {
    for family in [
        Family::Claim,
        Family::Allocate,
        Family::Deallocate,
        Family::Legacy,
        Family::Current,
        Family::Separate,
        Family::Failed,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYNTHETIC-funding.sqlite3");
        let venue = Venue::new(family);
        let running = engine(&path, venue.clone());
        let req = request(family);
        let original_input = req.input.clone();
        let result = running.execute(req).await.unwrap();
        assert_eq!(
            result.status,
            Status::Unknown,
            "family={family:?}, {:?}",
            result.error
        );
        assert!(result.effects.is_none());
        assert_eq!(result.preview.as_ref().unwrap().input, original_input);
        let store = gofin_store::Store::open(&path).unwrap();
        assert_eq!(
            store.unresolved_consumption_usd(&result.id).unwrap(),
            Some("2000000".into())
        );
        assert!(store.reservation_count(&result.id).unwrap() > 0);
        let proof = store
            .unresolved_consumption_proof(&result.id)
            .unwrap()
            .unwrap();
        assert_eq!(proof.submission, result.submission.clone().unwrap());
        assert_eq!(proof.costs.len(), 1);
        assert!(
            matches!(&proof.costs[0],FinancialEffect::Cost{amount,..} if amount.atoms.to_decimal(36)=="0.002")
        );
        assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 1);
        drop(store);
        drop(running);
        let restarted = engine(&path, venue.clone());
        let duplicate = restarted.status(&result.id, true).await.unwrap();
        assert_eq!(duplicate.status, Status::Unknown);
        assert!(duplicate.effects.is_none());
        let store = gofin_store::Store::open(&path).unwrap();
        assert_eq!(
            store.unresolved_consumption_usd(&result.id).unwrap(),
            Some("2000000".into())
        );
        assert!(store.reservation_count(&result.id).unwrap() > 0);
        assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn ambiguous_native_funding_fee_rows_never_become_consumption_or_settlement() {
    for family in [
        Family::Claim,
        Family::Allocate,
        Family::Legacy,
        Family::Current,
        Family::Separate,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYNTHETIC-ambiguous.sqlite3");
        let venue = Venue::new(family);
        venue.ambiguous.store(true, Ordering::SeqCst);
        let result = engine(&path, venue.clone())
            .execute(request(family))
            .await
            .unwrap();
        assert!(
            matches!(result.status, Status::Unknown | Status::Pending),
            "family={family:?} {:?}",
            result.error
        );
        assert!(result.effects.is_none());
        assert!(
            gofin_store::Store::open(&path)
                .unwrap()
                .unresolved_consumption_proof(&result.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn earn_joint_only_cap_violation_retains_zero_extra_watermark_and_both_proven_origins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-joint-earnt.sqlite3");
    let venue = Venue::new(Family::Joint);
    let running = engine(&path, venue.clone());
    let result = running.execute(request(Family::Joint)).await.unwrap();
    assert_eq!(result.status, Status::Unknown, "{:?}", result.error);
    assert!(result.effects.is_none());
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("0".into())
    );
    let proof = store
        .unresolved_consumption_proof(&result.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        proof.reason,
        gofin_domain::unresolved_consumption::ConsumptionReason::ProvenUnsettledCost
    );
    assert_eq!(proof.costs.len(), 2);
    assert!(proof.costs.iter().all(|effect|matches!(effect,FinancialEffect::Cost{amount,..}if amount.atoms.to_decimal(36)=="0.00075")));
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    drop(store);
    drop(running);
    let recovered = engine(&path, venue.clone())
        .status(&result.id, true)
        .await
        .unwrap();
    assert_eq!(recovered.status, Status::Unknown);
    assert!(recovered.effects.is_none());
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("0".into())
    );
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
}
struct RisingQuotes(AtomicUsize);
#[async_trait]
impl gofin_pricing::UsdPriceSource for RisingQuotes {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                "2000"
            } else {
                "4000"
            }
            .into(),
            source: format!("SYNTHETIC-rising-exact-{}", asset.reference),
            updated_at: now(),
            observed_at: now(),
        })
    }
}
#[tokio::test]
async fn funding_quote_refresh_above_frozen_usd_ceiling_aborts_before_native_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::new(Family::Claim);
    let profile = KrakenProfile::new(KrakenProduct::Spot, vec![account()], venue.clone())
        .unwrap()
        .with_prices(Arc::new(RisingQuotes(AtomicUsize::new(0))));
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::open(dir.path().join("SYNTHETIC-rise.sqlite3")).unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile))],
        Arc::new(Approval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let result = engine.execute(request(Family::Claim)).await.unwrap();
    assert_eq!(result.status, Status::Failed, "{:?}", result.error);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
    assert!(result.submission.is_none());
    assert_eq!(result.error.as_ref().unwrap().code, "VALUE_BOUND_EXCEEDED");
}

#[tokio::test]
async fn bounded_earn_allocation_and_deallocation_engine_settle_cash_position_and_origins_once() {
    for family in [Family::Allocate, Family::Deallocate] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYNTHETIC-earned.sqlite3");
        let venue = Venue::new(family);
        venue.bounded_fees.store(true, Ordering::SeqCst);
        let running = engine(&path, venue.clone());
        let result = running.execute(request(family)).await.unwrap();
        assert_eq!(
            result.status,
            Status::Completed,
            "family={family:?}, {:?}",
            result.error
        );
        let proof = result.effects.clone().unwrap();
        let costs = proof
            .effects
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::Cost { amount, .. } => Some(amount.atoms.clone()),
                _ => None,
            })
            .try_fold(Quantity::zero(), |sum, q| sum.checked_add(&q))
            .unwrap();
        assert_eq!(costs.to_decimal(36), "0.0005");
        let position_anchor = proof
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::PositionChange {
                    anchor: Some(anchor),
                    ..
                } => Some(anchor),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            position_anchor.principal.atoms.to_decimal(36),
            if matches!(family, Family::Allocate) {
                "1.09975"
            } else {
                "0.89975"
            }
        );
        assert_eq!(
            position_anchor.evidence["allocation"]["amount_allocated"]["total"]["native"],
            position_anchor.principal.atoms.to_decimal(36)
        );
        assert_eq!(
            position_anchor.evidence["ledgerReference"],
            "SYNTHETIC-earn-reference"
        );
        let origin = proof
            .effects
            .iter()
            .find(|effect| matches!(effect, FinancialEffect::OriginMovement { .. }))
            .unwrap();
        let FinancialEffect::OriginMovement {
            from,
            to,
            debit,
            credit,
        } = origin
        else {
            unreachable!()
        };
        assert_eq!(debit.asset, credit.asset);
        assert_eq!(debit.asset.reference, "XETH");
        if matches!(family, Family::Allocate) {
            assert!(matches!(from, AccountingLocation::Cash { .. }));
            assert!(matches!(to, AccountingLocation::Position { .. }));
            assert_eq!(debit.atoms.to_decimal(36), "0.1");
            assert_eq!(credit.atoms.to_decimal(36), "0.09975");
            assert!(
                !proof
                    .effects
                    .iter()
                    .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
            );
        } else {
            assert!(matches!(from, AccountingLocation::Position { .. }));
            assert!(matches!(to, AccountingLocation::Cash { .. }));
            assert_eq!(debit.atoms.to_decimal(36), "0.10025");
            assert_eq!(credit.atoms.to_decimal(36), "0.1");
            assert_eq!(
                proof
                    .effects
                    .iter()
                    .filter(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
                    .count(),
                1
            );
        }
        assert!(
            result
                .preview
                .as_ref()
                .unwrap()
                .valuation
                .as_ref()
                .unwrap()
                .movement_usd_micros
                >= Quantity::from_u64(200000000)
        );
        drop(running);
        let duplicate = engine(&path, venue.clone())
            .status(&result.id, true)
            .await
            .unwrap();
        assert_eq!(duplicate.effects, Some(proof));
        assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn exact_named_earn_deallocation_plan_keeps_declared_gross_movement_and_net_receipt() {
    use gofin_domain::financial_plans::*;
    for goal in ["0.09975", "0.1"] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::new(Family::Deallocate);
        venue.bounded_fees.store(true, Ordering::SeqCst);
        let path = dir.path().join("SYNTHETIC-plan.sqlite3");
        let running = engine(&path, venue.clone());
        let req = request(Family::Deallocate);
        let prepared = running.preview(&req.operation, &req.input).await.unwrap();
        let asset = prepared.intent.maximum_costs[0].amount.asset.clone();
        let spec = FinancialPlanSpec {
            summary: None,
            version: 1,
            name: "Synthetic exact Earn deallocation".into(),
            steps: vec![FinancialPlanStepSpec {
                step_id: "deallocate".into(),
                operation: req.operation.clone(),
                input: req.input.clone(),
                permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":[{"resource":account().resource,"asset":asset}]}),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: Some(ResourceAmount {
                        resource: account().resource,
                        amount: Amount {
                            asset: asset.clone(),
                            atoms: Quantity::from_decimal(goal, 36).unwrap(),
                        },
                    }),
                    operation_succeeded: true,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: prepared.intent.debits.clone(),
                maximum_costs: prepared.intent.maximum_costs.clone(),
            },
        };
        let consent = running
            .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                request_id: "SYNTHETIC-Earn-plan".into(),
                plan: spec,
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            })
            .await
            .unwrap();
        let result = running
            .execute_financial_plan_step(&consent.plan_id, "deallocate", req.clone())
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed, "{:?}", result.error);
        let original_effects = result.effects.clone();
        let prepared = result.preview.as_ref().unwrap();
        assert_eq!(prepared.binding[PLAN_METADATA_KEY]["exact_input"], true);
        assert!(
            prepared.valuation.as_ref().unwrap().movement_usd_micros
                >= Quantity::from_u64(200000000)
        );
        let status = running
            .financial_plan_status(&consent.plan_id)
            .await
            .unwrap();
        assert_eq!(status.usage.consumed_costs.len(), 2);
        assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
        assert_eq!(
            status.steps[0].state,
            if goal == "0.09975" {
                FinancialPlanStepProgressState::Completed
            } else {
                FinancialPlanStepProgressState::Blocked
            }
        );
        drop(running);
        let restarted = engine(&path, venue.clone());
        let recovered = restarted.status(&result.id, true).await.unwrap();
        assert_eq!(recovered.effects, original_effects);
        let mut second = req;
        second.request_id = "SYNTHETIC-distinct-second-noncash-attempt".into();
        assert!(
            restarted
                .execute_financial_plan_step(&consent.plan_id, "deallocate", second)
                .await
                .is_err(),
            "completed or substantively spent exact noncash source cannot execute again under this consent"
        );
        assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn funding_amount_fee_recipient_and_native_quote_token_cannot_retarget_original_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-bound-input.sqlite3");
    let venue = Venue::new(Family::Current);
    let running = engine(&path, venue.clone());
    let mut original = request(Family::Current);
    original.input["fee"] =
        json!({"fee_included":true,"quoted_fee":{"token":"SYNTHETIC-quote-original"}});
    let settled = running.execute(original.clone()).await.unwrap();
    assert_eq!(settled.status, Status::Unknown, "{:?}", settled.error);
    assert_eq!(
        settled.submission.as_ref().unwrap().data["binding"]["parameters"]["fee"]["quoted_fee"]["token"],
        "SYNTHETIC-quote-original"
    );
    for field in 0..4 {
        let mut changed = original.clone();
        match field {
            0 => changed.input["amount"]["asset_amount"]["amount"] = json!("0.102"),
            1 => changed.input["maxFee"] = json!("0.002"),
            2 => changed.input["address_id"] = json!("ABfffff-fffff-ffffff"),
            _ => changed.input["fee"]["quoted_fee"]["token"] = json!("SYNTHETIC-quote-other"),
        }
        assert!(
            running.execute(changed).await.is_err(),
            "an original request identity cannot authorize changed native economics"
        );
        let unchanged = running.status(&settled.id, false).await.unwrap();
        assert_eq!(unchanged.submission, settled.submission);
        assert_eq!(unchanged.preview, settled.preview);
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
}
