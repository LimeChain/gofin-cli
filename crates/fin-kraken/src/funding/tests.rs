//! Independent financial and original-identity lifecycle fixtures.
use super::*;

#[cfg(test)]
mod reconciliation {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use fin_exchange::kraken::DispatchGuard;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    struct Fixture(Mutex<VecDeque<(E, Value)>>);
    #[async_trait]
    impl KrakenTransport for Fixture {
        async fn request(
            &self,
            e: E,
            _: Option<&KrakenAccount>,
            _: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(!e.mutating(), "Reconciliation must never replay a write");
            assert!(guard.is_none());
            let (expected, response) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected transport call");
            assert_eq!(e, expected);
            Ok(KrakenResponse::Json(response))
        }
    }
    fn account() -> KrakenAccount {
        KrakenAccount {
            name: "synthetic-spot".into(),
            resource: ResourceId("kraken:synthetic-only".into()),
            credential_ref: "synthetic-key-only".into(),
            physical_identity: "synthetic-uid".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        }
    }
    fn profile(replies: Vec<(E, Value)>) -> (KrakenProfile, Arc<Fixture>) {
        let fixture = Arc::new(Fixture(Mutex::new(replies.into())));
        (
            KrakenProfile::new(KrakenProduct::Spot, vec![account()], fixture.clone()).unwrap(),
            fixture,
        )
    }
    fn withdrawal(verification: VerificationRequirement) -> WithdrawalBinding {
        WithdrawalBinding {
            api_version: "legacy-v0".into(),
            source: account().resource,
            asset: AssetId {
                network: "kraken".into(),
                reference: "XETH".into(),
                decimals: 36,
            },
            venue_asset_code: "XETH".into(),
            asset_class: "currency".into(),
            denomination: "rebased".into(),
            method_id: "Ether (Hex)".into(),
            network: "Ethereum".into(),
            address_id: "synthetic-destination".into(),
            address: "0x1111111111111111111111111111111111111111".into(),
            address_kind: "crypto".into(),
            destination_details: json!({"crypto":{"address":"0x1111111111111111111111111111111111111111"}}),
            tag: None,
            memo: None,
            gross: Quantity::from_decimal("0.101", 36).unwrap(),
            net: Quantity::from_decimal("0.1", 36).unwrap(),
            fee: Quantity::from_decimal("0.001", 36).unwrap(),
            fee_asset: AssetId {
                network: "kraken".into(),
                reference: "XETH".into(),
                decimals: 36,
            },
            fee_venue_code: "XETH".into(),
            fee_asset_class: "currency".into(),
            verification,
            destination: None,
        }
    }
    fn status() -> Value {
        json!({"error":[],"result":[{"refid":"original-venue-id","status":"Success","asset":"XETH","method":"Ether (Hex)","key":"synthetic-destination","info":"0x1111111111111111111111111111111111111111","amount":"0.1","fee":"0.001","txid":"0x2222222222222222222222222222222222222222222222222222222222222222"}]})
    }
    fn ledger() -> Value {
        json!({"error":[],"result":{"ledger":{"synthetic-ledger-id":{"refid":"original-venue-id","type":"withdrawal","asset":"XETH","amount":"-0.1","fee":"0.001","balance":"0.899"}}}})
    }
    fn submission(binding: Value) -> Submission {
        Submission {
            kind: "kraken-rest-v1".into(),
            reference: "durable-fin-id".into(),
            data: binding,
        }
    }
    fn record(submission: &Submission, dispatch: Option<Value>) -> OperationRecord {
        OperationRecord {
            id: "synthetic-operation".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: E::KrakenPrivatePostWithdraw.operation().into(),
            input: json!({}),
            status: Status::Unknown,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: Some(submission.clone()),
            dispatch_evidence: dispatch,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }
    #[tokio::test]
    async fn explicit_venue_only_books_proven_source_and_fee_without_destination_credit() {
        let (p, f) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, ledger()),
        ]);
        let b = withdrawal(VerificationRequirement::VenueOnly);
        let s = submission(json!({"account":account().name,"funding":b}));
        let result = p
            .reconcile_funding(
                &record(
                    &s,
                    Some(json!({"response":{"error":[],"result":{"refid":"original-venue-id"}}})),
                ),
                &s,
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(result.evidence["verification"]["level"], "venue_only");
        let effects = result.verified.unwrap();
        assert_eq!(effects.effects.len(), 2);
        assert!(matches!(
            effects.effects[0],
            FinancialEffect::CashDebit { .. }
        ));
        assert!(matches!(
            effects.effects[1],
            FinancialEffect::Cost {
                treatment: CostTreatment::IncludedInDebit,
                ..
            }
        ));
        assert!(f.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn response_loss_never_matches_amount_or_resubmits() {
        let (p, f) = profile(vec![]);
        let s = submission(
            json!({"account":account().name,"funding":withdrawal(VerificationRequirement::VenueOnly)}),
        );
        let result = p.reconcile_funding(&record(&s, None), &s).await.unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
        assert!(f.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn absent_original_reference_never_accepts_same_amount() {
        let mut response = status();
        response["result"][0]["refid"] = json!("another-withdrawal-id");
        let (p, _) = profile(vec![(E::KrakenPrivatePostWithdrawStatus, response)]);
        let result = p
            .reconcile_withdrawal(
                &account(),
                withdrawal(VerificationRequirement::VenueOnly),
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
    }
    #[tokio::test]
    async fn duplicate_debit_proofs_are_ambiguous() {
        let mut l = ledger();
        l["result"]["ledger"]["another-ledger-id"] =
            l["result"]["ledger"]["synthetic-ledger-id"].clone();
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, l),
        ]);
        let result = p
            .reconcile_withdrawal(
                &account(),
                withdrawal(VerificationRequirement::VenueOnly),
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
    }
    #[tokio::test]
    async fn success_without_exact_fee_and_source_ledger_is_unresolved() {
        let mut l = ledger();
        l["result"]["ledger"]["synthetic-ledger-id"]["fee"] = json!("0.002");
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, l),
        ]);
        let result = p
            .reconcile_withdrawal(
                &account(),
                withdrawal(VerificationRequirement::VenueOnly),
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
    }
    struct Receipt(Status);
    #[async_trait]
    impl KrakenRecipientVerifier for Receipt {
        async fn resolve(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
        ) -> Result<Option<KrakenDestination>> {
            unreachable!()
        }
        async fn verify(&self, r: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
            assert_eq!(r.minimum, "0.1");
            assert_eq!(r.maximum, "0.1");
            Ok(KrakenPayoutResult {
                status: self.0.clone(),
                amount: if self.0 == Status::Completed {
                    Some(Quantity::from_decimal("0.1", 18)?)
                } else {
                    None
                },
                evidence: json!({"synthetic":"canonical-receipt"}),
            })
        }
    }
    fn destination() -> KrakenDestination {
        KrakenDestination {
            enrolled: true,
            resource: ResourceId("ethereum:1:synthetic-enrolled-wallet".into()),
            address: withdrawal(VerificationRequirement::DestinationReceipt).address,
            chain_id: 1,
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            confirmations: 12,
        }
    }
    #[tokio::test]
    async fn default_receipt_requirement_never_downgrades_when_observer_missing() {
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, ledger()),
        ]);
        let mut b = withdrawal(VerificationRequirement::DestinationReceipt);
        b.destination = Some(destination());
        let result = p
            .reconcile_withdrawal(
                &account(),
                b,
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
    }
    #[tokio::test]
    async fn canonical_receipt_books_one_enrolled_usable_credit() {
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, ledger()),
        ]);
        let p = p.with_recipient_verifier(Arc::new(Receipt(Status::Completed)));
        let mut b = withdrawal(VerificationRequirement::DestinationReceipt);
        b.destination = Some(destination());
        let result = p
            .reconcile_withdrawal(
                &account(),
                b,
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        let effects = result.verified.unwrap().effects;
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, FinancialEffect::UsableCredit { .. }))
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn kraken_settled_withdrawal_waits_for_correlated_destination_receipt() {
        let mut venue_status = status();
        venue_status["result"][0]["status"] = json!("Settled");
        venue_status["result"][0]["time"] = json!(1791809714.125);
        let mut venue_ledger = ledger();
        venue_ledger["result"]["ledger"]["synthetic-ledger-id"]["time"] = json!(1791809714.125);
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, venue_status),
            (E::KrakenPrivatePostLedgers, venue_ledger),
        ]);
        let p = p.with_recipient_verifier(Arc::new(Receipt(Status::Completed)));
        let mut b = withdrawal(VerificationRequirement::DestinationReceipt);
        b.destination = Some(destination());
        let result = p
            .reconcile_withdrawal(
                &account(),
                b,
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(
            result.evidence["verification"]["level"],
            "destination_receipt"
        );
        assert_eq!(result.evidence["withdrawal"]["time"], "1791809714.125");
        assert_eq!(result.evidence["sourceLedger"]["time"], "1791809714.125");
        assert!(
            result
                .verified
                .unwrap()
                .effects
                .iter()
                .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
        );
    }
    #[tokio::test]
    async fn pending_recipient_receipt_does_not_create_any_terminal_effects() {
        let (p, _) = profile(vec![
            (E::KrakenPrivatePostWithdrawStatus, status()),
            (E::KrakenPrivatePostLedgers, ledger()),
        ]);
        let p = p.with_recipient_verifier(Arc::new(Receipt(Status::Pending)));
        let mut b = withdrawal(VerificationRequirement::DestinationReceipt);
        b.destination = Some(destination());
        let result = p
            .reconcile_withdrawal(
                &account(),
                b,
                &submission(json!({})),
                &json!({"result":{"refid":"original-venue-id"}}),
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Pending);
        assert!(result.verified.is_none());
    }
    #[tokio::test]
    async fn earn_response_loss_keeps_last_request_pointer_untrusted() {
        let (p, f) = profile(vec![]);
        let s = submission(json!({"account":account().name,"earn":{}}));
        let result = p.reconcile_funding(&record(&s, None), &s).await.unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(f.0.lock().unwrap().is_empty());
    }
    fn current_separate_fee(success: bool) -> (WithdrawalBinding, Value, Value, Value, Value) {
        let mut b = withdrawal(VerificationRequirement::VenueOnly);
        b.api_version = "funding-v1".into();
        b.method_id = "00000000-0000-0000-0000-000000000001".into();
        b.gross = b.net.clone();
        b.fee_asset.reference = "USDC".into();
        b.fee_venue_code = "USDC".into();
        let history = json!({"withdrawals":[{"withdrawal_id":"original-venue-id","status":if success{"success"}else{"failure"},"address_id":b.address_id,"method_id":b.method_id,"amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"},"fee":{"asset":{"class":"currency","name":"USDC"},"amount":"0.001"}}]});
        let response = json!({"withdrawal_id":"original-venue-id","gross_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"}},"net_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"}},"fee":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"0.001"}}});
        let mut principal = ledger();
        principal["result"]["ledger"]["synthetic-ledger-id"]["fee"] = json!("0");
        if !success {
            principal["result"]["ledger"]["refund"] = json!({"refid":"original-venue-id","type":"deposit","asset":"XETH","amount":"0.1","fee":"0"});
        }
        let fee = json!({"result":{"ledger":{"fee-debit":{"refid":"original-venue-id","type":"withdrawal","asset":"USDC","amount":"-0.001","fee":"0"}}}});
        (b, history, response, principal, fee)
    }
    #[tokio::test]
    async fn current_withdrawal_separate_fee_asset_requires_independent_debit_proof() {
        let (b, h, response, l, fee) = current_separate_fee(true);
        let (p, _) = profile(vec![
            (E::KrakenFundingGetFundingV1Withdrawals, h),
            (E::KrakenPrivatePostLedgers, l),
            (E::KrakenPrivatePostLedgers, fee),
        ]);
        let r = p
            .reconcile_withdrawal(&account(), b, &submission(json!({})), &response)
            .await
            .unwrap();
        assert_eq!(r.status, Status::Completed);
        let effects = r.verified.unwrap().effects;
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount, treatment:CostTreatment::AdditionalDebit,..}if amount.asset.reference=="USDC")));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, FinancialEffect::UsableCredit { .. }))
        );
    }
    #[tokio::test]
    async fn failed_current_withdrawal_returns_principal_and_books_only_proven_fee_asset_cost() {
        for refunded in [false, true] {
            let (b, h, response, l, mut fee) = current_separate_fee(false);
            if refunded {
                fee["result"]["ledger"]["fee-return"] = json!({"refid":"original-venue-id","type":"deposit","asset":"USDC","amount":"0.0009","fee":"0"});
            }
            let (p, _) = profile(vec![
                (E::KrakenFundingGetFundingV1Withdrawals, h),
                (E::KrakenPrivatePostLedgers, l),
                (E::KrakenPrivatePostLedgers, fee),
            ]);
            let r = p
                .reconcile_withdrawal(&account(), b, &submission(json!({})), &response)
                .await
                .unwrap();
            assert_eq!(r.status, Status::Failed);
            let effects = r.verified.unwrap().effects;
            assert!(
                matches!(&effects[0],FinancialEffect::CashDebit{amount,..}if amount.asset.reference=="USDC"&&amount.atoms==Quantity::from_decimal(if refunded{"0.0001"}else{"0.001"},36).unwrap())
            );
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, FinancialEffect::UsableCredit { .. }))
            );
        }
    }
    #[tokio::test]
    async fn missing_separate_fee_proof_or_changed_response_asset_never_settles() {
        let (b, h, response, l, _) = current_separate_fee(true);
        let (p, _) = profile(vec![
            (E::KrakenFundingGetFundingV1Withdrawals, h.clone()),
            (E::KrakenPrivatePostLedgers, l),
            (E::KrakenPrivatePostLedgers, json!({"result":{"ledger":{}}})),
        ]);
        assert_eq!(
            p.reconcile_withdrawal(&account(), b.clone(), &submission(json!({})), &response)
                .await
                .unwrap()
                .status,
            Status::Unknown
        );
        let mut wrong = response;
        wrong["fee"]["asset_amount"]["asset"]["name"] = json!("XETH");
        let (p, _) = profile(vec![(E::KrakenFundingGetFundingV1Withdrawals, h)]);
        assert_eq!(
            p.reconcile_withdrawal(&account(), b, &submission(json!({})), &wrong)
                .await
                .unwrap()
                .status,
            Status::Unknown
        );
    }
    #[tokio::test]
    async fn cross_product_transfer_requires_original_id_and_two_exact_ledger_legs() {
        let source = account();
        let mut destination = source.clone();
        destination.name = "synthetic-futures".into();
        destination.product = KrakenProduct::Derivatives;
        destination.resource = ResourceId("krakenfutures:synthetic-only".into());
        let transfer = TransferBinding {
            from: source.resource.clone(),
            to: ResourceId(format!("{}:wallet:cash", destination.resource.0)),
            asset: crate::observation::asset("XETH", KrakenProduct::Spot),
            credit_asset: crate::observation::asset("eth", KrakenProduct::Derivatives),
            amount: Quantity::from_decimal("0.1", 36).unwrap(),
            source_account: source.name.clone(),
            dispatch_account: source.name.clone(),
            dispatch_physical_identity: source.physical_identity.clone(),
            destination_account: destination.name.clone(),
            source_physical_identity: source.physical_identity.clone(),
            destination_physical_identity: destination.physical_identity.clone(),
            source_product: Some(source.product),
            destination_product: Some(destination.product),
            source_venue_uid: None,
            destination_venue_uid: Some(destination.physical_identity.clone()),
            relationship_iiban: Some(source.physical_identity.clone()),
            from_wallet: "Spot Wallet".into(),
            to_wallet: "Futures Wallet".into(),
        };
        for proven in [false, true] {
            let (p, f) = profile(vec![
                (
                    E::KrakenPrivatePostLedgers,
                    json!({"result":{"ledger":{"source-transfer":{"refid":"transfer-001","asset":"XETH","type":"transfer","amount":"-0.1","fee":"0"}}}}),
                ),
                (
                    E::KrakenfuturesHistoryGetAccountLog,
                    json!({"accountUid":destination.physical_identity,"logs":[{"booking_uid":"dest-booking-001","execution":if proven{"transfer-001"}else{"other-transfer"},"asset":"eth","margin_account":"cash","info":"cross-exchange transfer","old_balance":"1","new_balance":"1.1","fee":"0"}]}),
                ),
            ]);
            let p = p.with_related_accounts(vec![destination.clone()]);
            let result = p
                .reconcile_transfer(
                    &source,
                    transfer.clone(),
                    &submission(json!({})),
                    &json!({"result":"success","uid":"transfer-001"}),
                )
                .await
                .unwrap();
            assert_eq!(
                result.status,
                if proven {
                    Status::Completed
                } else {
                    Status::Pending
                }
            );
            if proven {
                assert!(
                    matches!(result.verified.unwrap().effects.as_slice(),[FinancialEffect::Transfer{to,..}]if to==&transfer.to)
                );
            } else {
                assert!(result.verified.is_none());
            }
            assert!(f.0.lock().unwrap().is_empty());
        }
        let (p, f) = profile(vec![]);
        let p = p.with_related_accounts(vec![destination.clone()]);
        assert_eq!(
            p.reconcile_transfer(
                &source,
                transfer.clone(),
                &submission(json!({})),
                &json!({"result":"success"})
            )
            .await
            .unwrap()
            .status,
            Status::Unknown
        );
        assert!(f.0.lock().unwrap().is_empty());
        destination.physical_identity = "changed-enrollment".into();
        let (p, f) = profile(vec![]);
        let p = p.with_related_accounts(vec![destination]);
        assert_eq!(
            p.reconcile_transfer(
                &source,
                transfer,
                &submission(json!({})),
                &json!({"result":{"refid":"transfer-001"}})
            )
            .await
            .unwrap()
            .status,
            Status::Unknown
        );
        assert!(f.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn frozen_wallet_alias_survives_optional_enrollment_change_but_deactivation_stays_unknown()
     {
        for active in [false, true] {
            let (p, _) = profile(vec![(
                E::KrakenPrivatePostListWalletAccounts,
                json!({"result":{"accounts":[{"account_id":"WSELECTED","flags":{"active":active,"user_defined":true},"status":if active{"active"}else{"disabled"},"type":"spot"}],"cursor":{"next":null}}}),
            )]);
            let claim = ClaimBinding {
                method_id: "fixture-method".into(),
                current_method_id: Some("fixture-method".into()),
                asset: crate::observation::asset("XETH", KrakenProduct::Spot),
                fee_asset: crate::observation::asset("XETH", KrakenProduct::Spot),
                fee: Quantity::zero(),
                addresses_before: vec![],
                ledger_before: Some(vec![]),
            };
            let s = submission(
                json!({"account":account().name,"walletAccountId":"WSELECTED","defaultWalletAccountId":"WDEFAULT","walletResource":format!("{}:wallet:WSELECTED",account().resource.0),"parameters":{"account_id":"WSELECTED"},"claim":claim}),
            );
            let response = json!({"address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}});
            let r = p
                .reconcile_funding(&record(&s, Some(json!({"response":response}))), &s)
                .await
                .unwrap();
            assert_eq!(
                r.status,
                if active {
                    Status::Completed
                } else {
                    Status::Unknown
                }
            );
            if active {
                assert!(
                    matches!(r.verified.unwrap().effects.as_slice(),[FinancialEffect::AuthorityChange{resource,..}]if resource.0.ends_with(":wallet:WSELECTED"))
                );
            } else {
                assert!(r.verified.is_none());
            }
        }
    }
    #[tokio::test]
    async fn legacy_default_binding_does_not_adopt_new_wallet_enrollment_alias() {
        let (mut p, f) = profile(vec![]);
        p.accounts[0].default_wallet_account_id = Some("WNEWDEFAULT".into());
        let claim = ClaimBinding {
            method_id: "legacy-method".into(),
            current_method_id: None,
            asset: crate::observation::asset("XETH", KrakenProduct::Spot),
            fee_asset: crate::observation::asset("XETH", KrakenProduct::Spot),
            fee: Quantity::zero(),
            addresses_before: vec![],
            ledger_before: Some(vec![]),
        };
        let s = submission(
            json!({"account":account().name,"endpoint":"private_post_create_deposit_address","parameters":{},"claim":claim}),
        );
        let r=p.reconcile_funding(&record(&s,Some(json!({"response":{"result":[{"address":"0x1111111111111111111111111111111111111111"}]}}))),&s).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert!(
            matches!(r.verified.unwrap().effects.as_slice(),[FinancialEffect::AuthorityChange{resource,..}]if resource==&account().resource)
        );
        assert!(f.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn setup_charge_requires_new_address_id_and_exact_correlated_fee_ledger() {
        for proven in [false, true] {
            let address =
                json!({"crypto":{"address":"0x1111111111111111111111111111111111111111"}});
            let ledger = if proven {
                json!({"result":{"ledger":{"setup-debit":{"type":"spend","refid":"new-address-001","asset":"USDC","amount":"-0.001","fee":"0"}}}})
            } else {
                json!({"result":{"ledger":{}}})
            };
            let (p, _) = profile(vec![
                (
                    E::KrakenFundingGetFundingV2DepositAddresses,
                    json!({"addresses":[{"id":"new-address-001","method_id":"00000000-0000-0000-0000-000000000001","address_details":address}]}),
                ),
                (E::KrakenPrivatePostLedgers, ledger),
            ]);
            let claim = ClaimBinding {
                method_id: "legacy-method".into(),
                current_method_id: Some("00000000-0000-0000-0000-000000000001".into()),
                asset: crate::observation::asset("XETH", KrakenProduct::Spot),
                fee_asset: crate::observation::asset("USDC", KrakenProduct::Spot),
                fee: Quantity::from_decimal("0.001", 36).unwrap(),
                addresses_before: vec![],
                ledger_before: Some(vec![]),
            };
            let s = submission(
                json!({"account":account().name,"endpoint":"private_post_create_deposit_address","parameters":{},"claim":claim}),
            );
            let r=p.reconcile_funding(&record(&s,Some(json!({"response":{"result":[{"address":"0x1111111111111111111111111111111111111111"}]}}))),&s).await.unwrap();
            assert_eq!(
                r.status,
                if proven {
                    Status::Completed
                } else {
                    Status::Pending
                }
            );
            if proven {
                assert!(r.verified.unwrap().effects.iter().any(|effect|matches!(effect,FinancialEffect::Cost{amount,treatment:CostTreatment::AdditionalDebit,..}if amount.asset.reference=="USDC")));
            } else {
                assert!(r.verified.is_none());
            }
        }
    }
    #[tokio::test]
    async fn setup_fee_baseline_rejects_old_charge_and_books_only_new_bounded_cost() {
        for (baseline, charged, expected) in [
            (None, "-0.001", Status::Unknown),
            (
                Some(vec!["setup-debit".to_owned()]),
                "-0.001",
                Status::Pending,
            ),
            (Some(vec![]), "-0.002", Status::Unknown),
            (Some(vec![]), "-0.0006", Status::Completed),
        ] {
            let (profile, _) = profile(vec![
                (
                    E::KrakenFundingGetFundingV2DepositAddresses,
                    json!({"addresses":[{"id":"reused-address-001","method_id":"00000000-0000-0000-0000-000000000001","address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}}]}),
                ),
                (
                    E::KrakenPrivatePostLedgers,
                    json!({"result":{"ledger":{"setup-debit":{"type":"spend","refid":"reused-address-001","asset":"USDC","amount":charged,"fee":"0"}}}}),
                ),
            ]);
            let claim = ClaimBinding {
                method_id: "legacy-method".into(),
                current_method_id: Some("00000000-0000-0000-0000-000000000001".into()),
                asset: crate::observation::asset("XETH", KrakenProduct::Spot),
                fee_asset: crate::observation::asset("USDC", KrakenProduct::Spot),
                fee: Quantity::from_decimal("0.001", 36).unwrap(),
                addresses_before: vec!["reused-address-001".into()],
                ledger_before: baseline,
            };
            let submission = submission(
                json!({"account":account().name,"endpoint":"private_post_create_deposit_address","parameters":{},"claim":claim}),
            );
            let reconciled=profile.reconcile_funding(&record(&submission,Some(json!({"response":{"result":[{"address":"0x1111111111111111111111111111111111111111"}]}}))),&submission).await.unwrap();
            assert_eq!(reconciled.status, expected);
            if expected == Status::Completed {
                assert!(reconciled.verified.unwrap().effects.iter().any(|effect| matches!(effect,FinancialEffect::Cost{amount,treatment:CostTreatment::AdditionalDebit,..}if amount.atoms==Quantity::from_decimal("0.0006",36).unwrap())));
            } else {
                assert!(reconciled.verified.is_none());
            }
        }
    }
    #[tokio::test]
    async fn fiat_deposit_address_claim_acknowledges_bank_identity_without_minting_credit() {
        let (p, _) = profile(vec![]);
        let asset = crate::observation::asset("ZUSD", KrakenProduct::Spot);
        let claim = ClaimBinding {
            method_id: "00000000-0000-0000-0000-000000000001".into(),
            current_method_id: Some("00000000-0000-0000-0000-000000000001".into()),
            asset: asset.clone(),
            fee_asset: asset,
            fee: Quantity::zero(),
            addresses_before: vec![],
            ledger_before: Some(vec![]),
        };
        let s = submission(json!({"account":account().name,"parameters":{},"claim":claim}));
        let r=p.reconcile_funding(&record(&s,Some(json!({"response":{"address_details":{"fiat":{"iban":"SYNTHETIC-IBAN-ONLY","bank":"Synthetic Bank"}}}}))),&s).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert!(matches!(
            r.verified.unwrap().effects.as_slice(),
            [FinancialEffect::AuthorityChange { .. }]
        ));
    }
    #[test]
    fn exact_quantities_and_ambiguous_scope_reject_without_rounding() {
        assert!(Quantity::from_decimal("0.0000000000000000000000000000000000001", 36).is_err());
        assert!(
            signed("-0.000000000000000000000000000000000001", 36)
                .unwrap()
                .is_negative()
        );
        assert!(
            unique(&[json!({"id":"same"}), json!({"id":"same"})], |v| v["id"]
                == "same")
            .is_err()
        );
    }
}

#[cfg(test)]
mod funding_options_tests {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use fin_exchange::kraken::DispatchGuard;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    struct Fixture {
        replies: Mutex<VecDeque<(E, Value)>>,
        requests: Mutex<Vec<(E, Value)>>,
        accounts: Mutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl KrakenTransport for Fixture {
        async fn request(
            &self,
            e: E,
            account: Option<&KrakenAccount>,
            p: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(!e.mutating());
            assert!(guard.is_none());
            if e == E::KrakenFundingGetFundingV1MethodsDirection {
                assert!(
                    matches!(p["direction"].as_str(), Some("deposit" | "withdraw")),
                    "Published native direction enum is deposit|withdraw, never withdrawal"
                );
            }
            if let Some(schema) = contracts::contract(e).schema.as_ref() {
                assert!(
                    jsonschema::validator_for(schema).unwrap().is_valid(&p),
                    "Request did not satisfy the independent published native schema: {e:?} {p}"
                );
            }
            self.requests.lock().unwrap().push((e, p));
            self.accounts
                .lock()
                .unwrap()
                .push(account.map(|a| a.name.clone()));
            let (expected, v) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected call");
            assert_eq!(e, expected);
            Ok(KrakenResponse::Json(v))
        }
    }
    fn account() -> KrakenAccount {
        KrakenAccount {
            name: "fixture-spot".into(),
            resource: ResourceId("kraken:fixture-only".into()),
            credential_ref: "fixture-key".into(),
            physical_identity: "fixture-id".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        }
    }
    fn profile(replies: Vec<(E, Value)>) -> (KrakenProfile, Arc<Fixture>) {
        let f = Arc::new(Fixture {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(vec![]),
            accounts: Mutex::new(vec![]),
        });
        (
            KrakenProfile::new(KrakenProduct::Spot, vec![account()], f.clone()).unwrap(),
            f,
        )
    }
    fn methods() -> Value {
        json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","method_name":"Ether (Hex)","asset":{"class":"currency","name":"XETH"},"network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":"Ethereum"},"minimum_amount":"0.01","maximum_amount":"2","fees":{"base":{"asset":{"class":"currency","name":"XETH"},"amount":"0.001"},"included":true}}]})
    }
    fn address(scope: Value) -> Value {
        json!({"addresses":[{"address_id":"ABabcde-abcde-abcdef","scope":scope,"verified":true,"address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}}]})
    }
    fn fee() -> Value {
        json!({"fee":{"asset":{"class":"currency","name":"XETH"},"amount":"0.001"},"gross_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.101"},"net_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.1"}})
    }
    fn assets() -> Value {
        json!({"result":{"XETH":{"decimals":18}}})
    }
    fn limits() -> Value {
        json!({"withdrawal_limits":[{"method_id":"00000000-0000-0000-0000-000000000001","maximum_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"1"}}]})
    }
    fn params() -> Value {
        json!({"scope":{"method_id":"00000000-0000-0000-0000-000000000001"},"address_id":"ABabcde-abcde-abcdef","amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.101"}},"fee":{"fee_included":true}})
    }
    fn replies(scope: Value) -> Vec<(E, Value)> {
        vec![
            (E::KrakenFundingGetFundingV1MethodsDirection, methods()),
            (E::KrakenFundingGetFundingV1Addresses, address(scope)),
            (E::KrakenFundingGetFundingV1FeesMethodId, fee()),
            (E::KrakenPublicGetAssets, assets()),
            (
                E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset,
                limits(),
            ),
            (E::KrakenPublicGetAssets, assets()),
        ]
    }
    struct PlanRecipient(bool);
    #[async_trait]
    impl KrakenRecipientVerifier for PlanRecipient {
        async fn resolve(
            &self,
            network: &str,
            coin: &str,
            contract: Option<&str>,
            address: &str,
            destination: Option<&str>,
        ) -> Result<Option<KrakenDestination>> {
            assert_eq!(network, "Ethereum");
            assert_eq!(coin, "XETH");
            assert!(contract.is_none());
            assert_eq!(address, "0x1111111111111111111111111111111111111111");
            assert_eq!(destination, Some("fixture-wallet"));
            Ok(self.0.then(|| KrakenDestination {
                enrolled: true,
                resource: ResourceId(format!("ethereum:1:{address}")),
                address: address.into(),
                chain_id: 1,
                asset: AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
                confirmations: 2,
            }))
        }
        async fn verify(&self, _: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
            panic!("Plan feasibility cannot claim a future receipt")
        }
    }
    #[tokio::test]
    async fn plan_feasibility_checks_withdrawal_destination_without_future_balance_or_account_limit()
     {
        use fin_domain::Adapter;
        use fin_exchange::{ExchangeAdapter, ExchangeProfile};
        for current in [false, true] {
            for observable in [false, true] {
                let mut responses = if current {
                    replies(json!({"method_id":"00000000-0000-0000-0000-000000000001"}))
                        .into_iter()
                        .filter(|(e, _)| {
                            *e != E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![
                        (
                            E::KrakenPrivatePostWithdrawInfo,
                            json!({"result":{"method":"Ether","amount":"0.1","fee":"0.001","limit":"0"}}),
                        ),
                        (
                            E::KrakenPrivatePostWithdrawAddresses,
                            json!({"result":[{"key":"fixture-destination","method":"Ether","verified":true,"address":"0x1111111111111111111111111111111111111111"}]}),
                        ),
                        (
                            E::KrakenPrivatePostWithdrawMethods,
                            json!({"result":[{"method":"Ether","network":"Ethereum","minimum":"0.01"}]}),
                        ),
                        (E::KrakenPublicGetAssets, assets()),
                        (E::KrakenPublicGetAssets, assets()),
                    ]
                };
                responses.insert(0, (E::KrakenPrivatePostGetApiKeyInfo, json!({"result":{"permissions":["query-funds","withdraw-funds","query-ledger"]}})));
                let (profile, wire) = profile(responses);
                let profile = profile.with_recipient_verifier(Arc::new(PlanRecipient(observable)));
                let mut input = if current {
                    params()
                } else {
                    json!({"asset":"XETH","key":"fixture-destination","amount":"0.101","max_fee":"0.002"})
                };
                input["account"] = json!("fixture-spot");
                input["requestId"] = json!("fixture-plan-withdrawal");
                input["destination"] = json!("fixture-wallet");
                if current {
                    input["fee"]["current_fee"] = json!({"max_fee":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.002"}}});
                }
                let operation = if current {
                    E::KrakenFundingPostFundingV1Withdrawals
                } else {
                    E::KrakenPrivatePostWithdraw
                }
                .operation();
                let adapter = ExchangeAdapter::new(profile);
                let result = Adapter::validate_plan_feasibility(&adapter, operation, &input).await;
                if observable {
                    assert!(
                        result
                            .unwrap()
                            .iter()
                            .any(|w| w.contains("remain deferred"))
                    );
                } else {
                    let failure = result.unwrap_err();
                    assert_eq!(
                        failure.code, "KRAKEN_DESTINATION_OBSERVER_UNSUPPORTED",
                        "current={current}: {failure:?}"
                    );
                }
                assert!(wire.replies.lock().unwrap().is_empty());
                assert!(
                    wire.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|(e, _)| !e.mutating()
                            && *e != E::KrakenPrivatePostBalanceEx
                            && *e != E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset)
                );
                // The same zero-limit legacy quote must still block at execution.
                if !current && observable {
                    wire.replies.lock().unwrap().extend(vec![
                        (E::KrakenPrivatePostGetApiKeyInfo, json!({"result":{"permissions":["query-funds","withdraw-funds","query-ledger"]}})),
                        (E::KrakenPrivatePostWithdrawInfo, json!({"result":{"method":"Ether","amount":"0.1","fee":"0.001","limit":"0"}})),
                        (E::KrakenPrivatePostWithdrawAddresses, json!({"result":[{"key":"fixture-destination","method":"Ether","verified":true,"address":"0x1111111111111111111111111111111111111111"}]})),
                        (E::KrakenPrivatePostWithdrawMethods, json!({"result":[{"method":"Ether","network":"Ethereum","minimum":"0.01"}]})),
                        (E::KrakenPublicGetAssets, assets()),
                    ]);
                    assert_eq!(
                        adapter
                            .profile()
                            .prepare(operation, &input)
                            .await
                            .unwrap_err()
                            .code,
                        "KRAKEN_WITHDRAWAL_LIMIT"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn current_fee_is_capped_on_venue_wire_and_destination_is_frozen() {
        let (p, f) = profile(replies(
            json!({"method_id":"00000000-0000-0000-0000-000000000001"}),
        ));
        let mut params = params();
        let b = p
            .prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut params,
                &json!({"verificationRequirement":"venue_only","maxFee":"0.002"}),
            )
            .await
            .unwrap();
        assert_eq!(
            params["fee"]["current_fee"]["max_fee"]["asset_amount"]["amount"],
            "0.001"
        );
        assert_eq!(params["expected_address"], b.address);
        assert_eq!(b.gross, b.net.checked_add(&b.fee).unwrap());
        assert!(f.replies.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn supplied_quote_token_and_fee_inclusion_survive_without_substitution() {
        let (p, f) = profile(replies(
            json!({"network_id":"00000000-0000-0000-0000-000000000002"}),
        ));
        let mut params = params();
        params["fee"] = json!({"fee_included":false,"quoted_fee":{"token":"fixture-quote-token"}});
        params["amount"]["asset_amount"]["amount"] = json!("0.1");
        p.prepare_withdrawal(
            &account(),
            E::KrakenFundingPostFundingV1Withdrawals,
            &mut params,
            &json!({"verificationRequirement":"venue_only"}),
        )
        .await
        .unwrap();
        assert_eq!(params["fee"]["quoted_fee"]["token"], "fixture-quote-token");
        assert_eq!(params["fee"]["fee_included"], false);
        let req = f.requests.lock().unwrap();
        let fee_req = &req
            .iter()
            .find(|(e, _)| *e == E::KrakenFundingGetFundingV1FeesMethodId)
            .unwrap()
            .1;
        assert_eq!(fee_req["withdrawal_fee_token"], "fixture-quote-token");
        assert_eq!(fee_req["fee_included"], false);
    }
    #[tokio::test]
    async fn exact_network_group_membership_is_resolved_independently() {
        let mut r = replies(json!({"network_group_id":"00000000-0000-0000-0000-000000000003"}));
        r.insert(2,(E::KrakenFundingGetFundingV1Networks,json!({"network_groups":[{"network_group_id":"00000000-0000-0000-0000-000000000003","network_ids":["00000000-0000-0000-0000-000000000002"]}]})));
        let (p, _) = profile(r);
        p.prepare_withdrawal(
            &account(),
            E::KrakenFundingPostFundingV1Withdrawals,
            &mut params(),
            &json!({"verificationRequirement":"venue_only"}),
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn network_scope_cannot_choose_between_two_asset_methods() {
        let mut m = methods();
        let mut second = m["methods"][0].clone();
        second["method_id"] = json!("00000000-0000-0000-0000-000000000009");
        m["methods"].as_array_mut().unwrap().push(second);
        let (p, _) = profile(vec![(E::KrakenFundingGetFundingV1MethodsDirection, m)]);
        let mut input = params();
        input["scope"] = json!({"network_id":"00000000-0000-0000-0000-000000000002"});
        assert_eq!(
            p.prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut input,
                &json!({"verificationRequirement":"venue_only"})
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_EVIDENCE_AMBIGUOUS"
        );
    }
    #[tokio::test]
    async fn native_and_fin_fee_caps_reject_before_any_mutation() {
        let mut r = replies(json!({"method_id":"00000000-0000-0000-0000-000000000001"}));
        r.pop();
        let (p, _) = profile(r);
        let error = p
            .prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut params(),
                &json!({"verificationRequirement":"venue_only","maxFee":"0.0005"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "KRAKEN_FEE_LIMIT");
    }
    #[tokio::test]
    async fn invalid_same_currency_fee_arithmetic_is_never_rounded() {
        let mut r = replies(json!({"method_id":"00000000-0000-0000-0000-000000000001"}));
        r[2].1["gross_amount"]["amount"] = json!("0.101000000000000000000000000000000001");
        let (p, _) = profile(r);
        assert_eq!(
            p.prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut params(),
                &json!({"verificationRequirement":"venue_only"})
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_FEE_ACCOUNTING_INVALID"
        );
    }
    #[tokio::test]
    async fn fiat_saved_destination_requires_explicit_venue_only_and_binds_material_bank_identity()
    {
        fn bank_replies() -> Vec<(E, Value)> {
            let mut rows = replies(json!({"method_id":"00000000-0000-0000-0000-000000000001"}));
            rows[0].1["methods"][0]["method_name"] = json!("Synthetic Bank Route");
            rows[0].1["methods"][0]
                .as_object_mut()
                .unwrap()
                .remove("network");
            rows[0].1["methods"][0]["asset"]["name"] = json!("ZUSD");
            rows[1].1["addresses"][0]["address_details"] = json!({"fiat":{"iban":"SYNTHETIC-IBAN-ONLY","bank":"Synthetic Bank","routing":"FIXTURE-ROUTE","name_on_account":"Synthetic Recipient","cvc":"NEVER-PERSIST-THIS"}});
            for field in ["fee", "gross_amount", "net_amount"] {
                rows[2].1[field]["asset"]["name"] = json!("ZUSD");
            }
            rows[3].1 = json!({"result":{"ZUSD":{"decimals":4}}});
            rows[4].1["withdrawal_limits"][0]["maximum_amount"]["asset"]["name"] = json!("ZUSD");
            rows[5].1 = rows[3].1.clone();
            rows
        }
        let mut parameters = params();
        parameters["amount"]["asset_amount"]["asset"]["name"] = json!("ZUSD");
        let (p, f) = profile(bank_replies());
        let b = p
            .prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut parameters,
                &json!({"verificationRequirement":"venue_only"}),
            )
            .await
            .unwrap();
        assert_eq!(b.address_kind, "fiat");
        assert_eq!(b.address, "SYNTHETIC-IBAN-ONLY");
        assert_eq!(b.destination_details["fiat"]["routing"], "FIXTURE-ROUTE");
        assert!(b.destination_details["fiat"].get("cvc").is_none());
        assert!(parameters.get("expected_address").is_none());
        assert!(f.replies.lock().unwrap().is_empty());
        let (p, _) = profile(bank_replies().into_iter().take(2).collect());
        let mut parameters = params();
        parameters["amount"]["asset_amount"]["asset"]["name"] = json!("ZUSD");
        assert_eq!(
            p.prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut parameters,
                &json!({})
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_FIAT_RECEIPT_OBSERVER_UNSUPPORTED"
        );
    }
    #[tokio::test]
    async fn earn_strategy_later_cursor_page_resolves_exact_identity_after_complete_traversal() {
        let target = json!({"id":"ES-SYNTHETIC-TARGET","asset":"XETH","can_allocate":true,"allocation_fee":"0"});
        let (p, f) = profile(vec![
            (
                E::KrakenPrivatePostEarnStrategies,
                json!({"result":{"items":[{"id":"ES-SYNTHETIC-OTHER","asset":"XXBT"}],"next_cursor":"SYNTHETIC-next"}}),
            ),
            (
                E::KrakenPrivatePostEarnStrategies,
                json!({"result":{"items":[target.clone()],"next_cursor":null}}),
            ),
        ]);
        assert_eq!(
            p.earn_strategy(&account(), "ES-SYNTHETIC-TARGET")
                .await
                .unwrap(),
            target
        );
        let requests = f.requests.lock().unwrap();
        assert_eq!(requests[0].1, json!({"limit":500,"ascending":true}));
        assert_eq!(
            requests[1].1,
            json!({"limit":500,"ascending":true,"cursor":"SYNTHETIC-next"})
        );
        assert!(f.replies.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn earn_strategy_duplicate_conflict_missing_and_cursor_ambiguity_never_adopt_partial_target()
     {
        for (second, code) in [
            (
                json!({"items":[{"id":"ES-SYNTHETIC-TARGET","asset":"XETH"}],"next_cursor":null}),
                "KRAKEN_EARN_AMBIGUOUS",
            ),
            (
                json!({"items":[{"id":"ES-SYNTHETIC-TARGET","asset":"XXBT"}],"next_cursor":null}),
                "KRAKEN_EARN_AMBIGUOUS",
            ),
            (
                json!({"items":[{"id":"ES-SYNTHETIC-OTHER"}],"next_cursor":"SYNTHETIC-next"}),
                "KRAKEN_EARN_COVERAGE_INCOMPLETE",
            ),
            (
                json!({"items":[],"next_cursor":1}),
                "KRAKEN_EARN_COVERAGE_INCOMPLETE",
            ),
        ] {
            let (p, f) = profile(vec![
                (
                    E::KrakenPrivatePostEarnStrategies,
                    json!({"result":{"items":[{"id":"ES-SYNTHETIC-TARGET","asset":"XETH"}],"next_cursor":"SYNTHETIC-next"}}),
                ),
                (E::KrakenPrivatePostEarnStrategies, json!({"result":second})),
            ]);
            assert_eq!(
                p.earn_strategy(&account(), "ES-SYNTHETIC-TARGET")
                    .await
                    .unwrap_err()
                    .code,
                code
            );
            assert!(f.replies.lock().unwrap().is_empty());
        }
        let (p, _) = profile(vec![(
            E::KrakenPrivatePostEarnStrategies,
            json!({"result":{"items":[],"next_cursor":null}}),
        )]);
        assert_eq!(
            p.earn_strategy(&account(), "ES-SYNTHETIC-MISSING")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_EARN_STRATEGY_UNAVAILABLE"
        );
    }
    #[tokio::test]
    async fn earn_strategy_page_and_total_bounds_never_adopt_nonexhaustive_identity() {
        let rows = (0..501)
            .map(|i| json!({"id":format!("ES-SYNTHETIC-{i}")}))
            .collect::<Vec<_>>();
        let (p, _) = profile(vec![(
            E::KrakenPrivatePostEarnStrategies,
            json!({"result":{"items":rows}}),
        )]);
        assert_eq!(
            p.earn_strategy(&account(), "ES-SYNTHETIC-0")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_EARN_COVERAGE_INCOMPLETE"
        );
        let pages=(0..20).map(|i|(E::KrakenPrivatePostEarnStrategies,json!({"result":{"items":[{"id":format!("ES-SYNTHETIC-{i}")}],"next_cursor":format!("SYNTHETIC-next-{i}")}}))).collect();
        let (p, f) = profile(pages);
        assert_eq!(
            p.earn_strategy(&account(), "ES-SYNTHETIC-0")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_EARN_COVERAGE_INCOMPLETE"
        );
        assert_eq!(f.requests.lock().unwrap().len(), 20);
        assert!(f.replies.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn derivatives_transfer_never_labels_deficit_repayment_as_plain_usable_credit() {
        for source_negative in [true, false] {
            let mut futures = account();
            futures.product = KrakenProduct::Derivatives;
            futures.resource = ResourceId("krakenfutures:SYNTHETIC-deficit-transfer".into());
            futures.physical_identity = "00000000-0000-0000-0000-000000000051".into();
            let mut rows = vec![
                (
                    E::KrakenPublicGetAssets,
                    json!({"result":{"ZUSD":{"decimals":4}}}),
                ),
                (
                    E::KrakenfuturesAuthGetApiKeysV3Check,
                    json!({"accountUid":futures.physical_identity,"iiban":null,"permissions":{"general":"READ_ONLY","transfer":"FULL_ACCESS"}}),
                ),
                (
                    E::KrakenfuturesPrivateGetAccounts,
                    json!({"accounts":{"cash":{"balances":{"USD":if source_negative{"-0.01"}else{"1"}}}}}),
                ),
            ];
            if !source_negative {
                rows.push((E::KrakenfuturesPrivateGetAccounts,json!({"accounts":{"flex":{"currencies":{"USD":{"quantity":"-0.01","available":"-0.01"}}}}})));
            }
            let (_, f) = profile(rows);
            let p =
                KrakenProfile::new(KrakenProduct::Derivatives, vec![futures.clone()], f.clone())
                    .unwrap();
            let failure = p
                .prepare_transfer(
                    &futures,
                    E::KrakenfuturesPrivatePostTransfer,
                    &json!({"fromAccount":"cash","toAccount":"flex","unit":"USD","amount":"0.1"}),
                    &json!({}),
                )
                .await
                .unwrap_err();
            assert_eq!(
                failure.code,
                if source_negative {
                    "KRAKEN_TRANSFER_SOURCE_LIABILITY_UNSUPPORTED"
                } else {
                    "KRAKEN_COLLATERAL_LIABILITY_SETTLEMENT_UNPROVEN"
                }
            );
            assert!(f.replies.lock().unwrap().is_empty());
            assert_eq!(
                f.requests.lock().unwrap().len(),
                if source_negative { 3 } else { 4 }
            );
        }
    }
    #[tokio::test]
    async fn product_relationship_uses_iiban_while_futures_ledger_identity_remains_its_uuid() {
        for related in [true, false] {
            let mut spot = account();
            spot.physical_identity = "AA00SYNTHETIC".into();
            spot.identity_verified = false;
            let mut futures = spot.clone();
            futures.name = "fixture-futures".into();
            futures.resource = ResourceId("krakenfutures:fixture-only".into());
            futures.product = KrakenProduct::Derivatives;
            futures.physical_identity = "00000000-0000-0000-0000-000000000011".into();
            let mut rows = vec![
                (E::KrakenPublicGetAssets, assets()),
                (
                    E::KrakenPrivatePostGetApiKeyInfo,
                    json!({"result":{"iban":"AA00 SYNTHETIC","permissions":["query-funds","query-ledger","withdraw-funds"]}}),
                ),
                (
                    E::KrakenfuturesAuthGetApiKeysV3Check,
                    json!({"apiKey":"SYNTHETIC-DO-NOT-PERSIST","accountUid":futures.physical_identity,"iiban":if related{"AA00 SYNTHETIC"}else{"AA00 OTHER"},"permissions":{"general":"READ_ONLY","transfer":"NO_ACCESS"}}),
                ),
            ];
            if related {
                rows.extend([
                    (
                        E::KrakenPrivatePostBalanceEx,
                        json!({"result":{"XETH":{"balance":"1","hold_trade":"0"}}}),
                    ),
                    (
                        E::KrakenfuturesPrivateGetAccounts,
                        json!({"accounts":{"cash":{"balances":{"eth":"1"}}}}),
                    ),
                ]);
            }
            let (_, f) = profile(rows);
            let p = KrakenProfile::new(KrakenProduct::Spot, vec![spot.clone()], f.clone())
                .unwrap()
                .with_related_accounts(vec![futures.clone()]);
            let result=p.prepare_transfer(&spot,E::KrakenPrivatePostWalletTransfer,&json!({"asset":"XETH","amount":"0.1","from":"Spot Wallet","to":"Futures Wallet"}),&json!({"destinationAccount":"fixture-futures"})).await;
            if related {
                let b = result.unwrap();
                assert_eq!(b.relationship_iiban.as_deref(), Some("AA00SYNTHETIC"));
                assert_eq!(
                    b.destination_venue_uid.as_deref(),
                    Some(futures.physical_identity.as_str())
                );
                assert_ne!(b.source_physical_identity, b.destination_physical_identity);
                assert!(
                    !serde_json::to_string(&b)
                        .unwrap()
                        .contains("SYNTHETIC-DO-NOT-PERSIST")
                );
            } else {
                assert_eq!(
                    result.unwrap_err().code,
                    "KRAKEN_CROSS_PRODUCT_IDENTITY_UNVERIFIED"
                );
            }
            assert!(f.replies.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn master_subaccount_transfer_separates_dispatch_authority_from_child_custody() {
        for owned in [true, false] {
            let mut master = account();
            master.product = KrakenProduct::Derivatives;
            master.physical_identity = "00000000-0000-0000-0000-000000000021".into();
            master.resource = ResourceId("krakenfutures:master-fixture".into());
            let mut child = master.clone();
            child.name = "fixture-child".into();
            child.physical_identity = "00000000-0000-0000-0000-000000000022".into();
            child.resource = ResourceId("krakenfutures:child-fixture".into());
            let mut rows = vec![
                (E::KrakenPublicGetAssets, assets()),
                (
                    E::KrakenfuturesAuthGetApiKeysV3Check,
                    json!({"accountUid":master.physical_identity,"iiban":null,"permissions":{"general":"READ_ONLY","transfer":"FULL_ACCESS"}}),
                ),
                (
                    E::KrakenfuturesAuthGetApiKeysV3Check,
                    json!({"accountUid":child.physical_identity,"iiban":null,"permissions":{"general":"READ_ONLY","transfer":"NO_ACCESS"}}),
                ),
                (
                    E::KrakenfuturesPrivateGetSubaccounts,
                    json!({"result":"success","masterAccountUid":master.physical_identity,"subaccounts":[{"accountUid":if owned{child.physical_identity.as_str()}else{"other-uid"}}]}),
                ),
            ];
            if owned {
                rows.extend([
                    (
                        E::KrakenfuturesPrivateGetAccounts,
                        json!({"accounts":{"cash":{"balances":{"eth":"1"}}}}),
                    ),
                    (
                        E::KrakenfuturesPrivateGetAccounts,
                        json!({"accounts":{"flex":{"currencies":{"eth":{"quantity":"0"}}}}}),
                    ),
                ]);
            }
            let (_, f) = profile(rows);
            let p = KrakenProfile::new(KrakenProduct::Derivatives, vec![master.clone()], f.clone())
                .unwrap()
                .with_related_accounts(vec![child.clone()]);
            let result=p.prepare_transfer(&master,E::KrakenfuturesPrivatePostTransferSubaccount,&json!({"fromUser":child.physical_identity,"toUser":master.physical_identity,"fromAccount":"cash","toAccount":"flex","unit":"eth","amount":"0.1"}),&json!({"sourceAccount":"fixture-child","destinationAccount":master.name})).await;
            if owned {
                let b = result.unwrap();
                assert_eq!(b.dispatch_account, master.name);
                assert_eq!(b.source_account, child.name);
                assert_eq!(b.from.0, "krakenfutures:child-fixture:wallet:cash");
                assert_eq!(b.to.0, "krakenfutures:master-fixture:wallet:flex");
                assert_eq!(
                    f.accounts.lock().unwrap()[4].as_deref(),
                    Some("fixture-child")
                );
            } else {
                assert_eq!(
                    result.unwrap_err().code,
                    "KRAKEN_SUBACCOUNT_RELATIONSHIP_UNVERIFIED"
                );
            }
            assert!(f.replies.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn derivative_transfer_full_access_is_independent_of_readable_general_permission() {
        let (p, _) = profile(vec![(
            E::KrakenfuturesAuthGetApiKeysV3Check,
            json!({"accountUid":"00000000-0000-0000-0000-000000000031","iiban":null,"permissions":{"general":"FULL_ACCESS","transfer":"READ_ONLY"}}),
        )]);
        let mut a = account();
        a.product = KrakenProduct::Derivatives;
        a.physical_identity = "00000000-0000-0000-0000-000000000031".into();
        assert_eq!(
            p.transfer_identity(&a, true).await.unwrap_err().code,
            "KRAKEN_PERMISSION_DENIED"
        );
    }
    #[tokio::test]
    async fn venue_amount_precision_is_checked_before_normalizing_cash_to_domain_scale() {
        let (p, f) = profile(vec![(
            E::KrakenPublicGetAssets,
            json!({"result":{"ZUSD":{"aclass":"currency","decimals":4}}}),
        )]);
        assert_eq!(
            p.funding_asset_for_amount(&account(), "ZUSD", "currency", "rebased", "0.00001")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_FUNDING_AMOUNT_PRECISION"
        );
        assert!(f.replies.lock().unwrap().is_empty());
        let (p, _) = profile(vec![(
            E::KrakenPublicGetAssets,
            json!({"result":{"ZUSD":{"aclass":"currency","decimals":4}}}),
        )]);
        let asset = p
            .funding_asset_for_amount(&account(), "ZUSD", "currency", "rebased", "0.0001")
            .await
            .unwrap();
        assert_eq!(asset.decimals, 36);
        assert_eq!(asset.reference, "ZUSD");
    }
    #[tokio::test]
    async fn native_positive_decimal_grammar_and_trailing_zeros_preserve_exact_wire() {
        assert_eq!(
            native_quantity("+.1", 4).unwrap(),
            Quantity::from_decimal("0.1", 4).unwrap()
        );
        assert_eq!(
            native_quantity("1.000000", 4).unwrap(),
            Quantity::from_decimal("1", 4).unwrap()
        );
        assert_eq!(
            native_quantity("1.", 4).unwrap(),
            Quantity::from_decimal("1", 4).unwrap()
        );
        for bad in ["1..0", "--0.1", "-+0.1", "NaN", "1e4", "+", "."] {
            assert!(native_quantity(bad, 36).is_err());
        }
        assert!(signed("--0.1", 36).is_err());
        assert!(signed("-+0.1", 36).is_err());
        let (p, _) = profile(replies(
            json!({"method_id":"00000000-0000-0000-0000-000000000001"}),
        ));
        let mut parameters = params();
        parameters["amount"]["asset_amount"]["amount"] = json!("+.101000000000000000000");
        let binding = p
            .prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut parameters,
                &json!({"verificationRequirement":"venue_only"}),
            )
            .await
            .unwrap();
        assert_eq!(binding.gross, Quantity::from_decimal("0.101", 36).unwrap());
        assert_eq!(
            parameters["amount"]["asset_amount"]["amount"],
            "+.101000000000000000000"
        );
    }
    fn owned_wallets() -> Value {
        json!({"result":{"accounts":[
            {"account_id":"WDEFAULT","flags":{"active":true,"user_defined":false},"status":"active","type":"main"},
            {"account_id":"WSELECTED","flags":{"active":true,"user_defined":true},"status":"active","type":"spot"}
        ],"cursor":{"next":null}}})
    }
    #[tokio::test]
    async fn funding_wallet_scope_is_authenticated_and_primary_alias_is_not_duplicated() {
        let (p, f) = profile(vec![
            (E::KrakenPrivatePostListWalletAccounts, owned_wallets()),
            (
                E::KrakenPrivatePostBalanceEx,
                json!({"result":{"XETH":{"balance":"1","hold_trade":"0.1","credit_used":"0.2"}}}),
            ),
        ]);
        let mut a = account();
        a.default_wallet_account_id = Some("WDEFAULT".into());
        let scopes = p.account_scopes(&a).await.unwrap();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0].resource, a.resource);
        assert_eq!(
            scopes[1].resource,
            ResourceId(format!("{}:wallet:WSELECTED", a.resource.0))
        );
        assert_eq!(
            p.free(
                &scopes[1],
                &crate::observation::asset("XETH", KrakenProduct::Spot)
            )
            .await
            .unwrap(),
            Quantity::from_decimal("0.7", 36).unwrap()
        );
        let calls = f.requests.lock().unwrap();
        assert_eq!(calls[0].1, json!({}));
        assert_eq!(calls[1].1, json!({"account_id":"WSELECTED"}));
    }
    #[tokio::test]
    async fn funding_wallet_selection_requires_human_default_relationship_and_owned_active_id() {
        let (p, f) = profile(vec![]);
        assert_eq!(
            p.funding_wallet(&account(), "WSELECTED")
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEFAULT_WALLET_RELATIONSHIP_UNVERIFIED"
        );
        assert!(f.requests.lock().unwrap().is_empty());
        for id in ["WNOTOWNED", "WDISABLED"] {
            let mut rows = owned_wallets();
            rows["result"]["accounts"].as_array_mut().unwrap().push(json!({"account_id":"WDISABLED","flags":{"active":false,"user_defined":true},"status":"disabled","type":"spot"}));
            let (p, _) = profile(vec![(E::KrakenPrivatePostListWalletAccounts, rows)]);
            let mut a = account();
            a.default_wallet_account_id = Some("WDEFAULT".into());
            assert_eq!(
                p.funding_wallet(&a, id).await.unwrap_err().code,
                "KRAKEN_WALLET_ACCOUNT_UNAVAILABLE"
            );
        }
    }
    #[tokio::test]
    async fn funding_wallet_duplicate_or_unpageable_relationship_never_authorizes_scope() {
        let mut duplicate = owned_wallets();
        let repeated = duplicate["result"]["accounts"][0].clone();
        duplicate["result"]["accounts"]
            .as_array_mut()
            .unwrap()
            .push(repeated);
        let mut incomplete = owned_wallets();
        incomplete["result"]["cursor"]["next"] = json!("unpageable-cursor");
        for (rows, code) in [
            (duplicate, "KRAKEN_WALLET_IDENTITY_INVALID"),
            (incomplete, "KRAKEN_WALLET_COVERAGE_INCOMPLETE"),
        ] {
            let (p, _) = profile(vec![(E::KrakenPrivatePostListWalletAccounts, rows)]);
            let mut a = account();
            a.default_wallet_account_id = Some("WDEFAULT".into());
            assert_eq!(
                p.funding_wallet(&a, "WSELECTED").await.unwrap_err().code,
                code
            );
        }
    }
    fn earn_binding() -> EarnBinding {
        EarnBinding {
            accounting_version: 1,
            source: account().resource,
            strategy: "fixture-strategy".into(),
            asset: AssetId {
                network: "kraken".into(),
                reference: "XETH".into(),
                decimals: 36,
            },
            amount: Quantity::from_decimal("0.1", 36).unwrap(),
            allocate: true,
            total_before: Quantity::from_decimal("1", 36).unwrap(),
            ledger_before: vec!["old-ledger".into()],
            lock_type: json!({"type":"instant"}),
            max_fee: Quantity::from_decimal("0.002", 36).unwrap(),
            minimum_usd: None,
            status_before: json!({"result":{"pending":false}}),
            allocations_before: json!({"result":{"items":[{"strategy_id":"fixture-strategy","amount_allocated":{"total":{"native":"1"}}}]}}),
        }
    }
    fn earn_rows() -> Value {
        json!({"result":{"ledger":{"cash-leg":{"type":"transfer","refid":"earn-reference","asset":"XETH","amount":"-0.1","fee":"0.001"},"position-leg":{"type":"transfer","refid":"earn-reference","asset":"XETH.B","amount":"0.1","fee":"0"}}}})
    }
    fn earn_replies() -> Vec<(E, Value)> {
        vec![
            (
                E::KrakenPrivatePostEarnAllocateStatus,
                json!({"result":{"pending":false}}),
            ),
            (
                E::KrakenPrivatePostEarnAllocations,
                json!({"result":{"items":[{"strategy_id":"fixture-strategy","amount_allocated":{"total":{"native":"1.1"}}}]}}),
            ),
            (E::KrakenPrivatePostLedgers, earn_rows()),
        ]
    }
    #[tokio::test]
    async fn legacy_earn_accounting_decode_never_injects_new_provenance_or_fee_authority() {
        let mut value = serde_json::to_value(earn_binding()).unwrap();
        value.as_object_mut().unwrap().remove("accounting_version");
        let old: EarnBinding = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(old.accounting_version, 0);
        let (p, f) = profile(vec![]);
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "SYNTHETIC-old-contract".into(),
            data: json!({"originalEarn":value}),
        };
        let observed = p
            .reconcile_earn(&account(), old, &s, &json!({"result":true}))
            .await
            .unwrap();
        assert_eq!(observed.status, Status::Unknown);
        assert!(observed.verified.is_none());
        assert!(f.requests.lock().unwrap().is_empty());
        assert_eq!(s.data["originalEarn"], value);
        let mut zero: EarnBinding = serde_json::from_value(value).unwrap();
        zero.max_fee = Quantity::zero();
        let mut responses = earn_replies();
        responses[2].1["result"]["ledger"]["cash-leg"]["fee"] = json!("0");
        let (p, f) = profile(responses);
        let observed = p
            .reconcile_earn(&account(), zero, &s, &json!({"result":true}))
            .await
            .unwrap();
        assert_eq!(observed.status, Status::Completed);
        let effects = observed.verified.unwrap();
        assert!(!effects.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::OriginMovement { .. } | FinancialEffect::Cost { .. }
        )));
        assert!(f.replies.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn acknowledged_serialized_earn_uses_distinct_cash_position_and_exact_fee() {
        let (p, _) = profile(earn_replies());
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "fixture-request".into(),
            data: json!({}),
        };
        let r = p
            .reconcile_earn(&account(), earn_binding(), &s, &json!({"result":true}))
            .await
            .unwrap();
        assert_eq!(r.status, Status::Completed);
        let effects = r.verified.unwrap().effects;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, FinancialEffect::PositionChange { .. }))
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            FinancialEffect::Cost {
                treatment: CostTreatment::AdditionalDebit,
                ..
            }
        )));
    }
    #[tokio::test]
    async fn idle_earn_status_without_unique_coherent_ledger_remains_unknown() {
        let mut responses = earn_replies();
        let mut duplicate = earn_rows()["result"]["ledger"]["cash-leg"].clone();
        duplicate["refid"] = json!("other-earn-reference");
        responses[2].1["result"]["ledger"]["other-cash-leg"] = duplicate;
        let mut positive = earn_rows()["result"]["ledger"]["position-leg"].clone();
        positive["refid"] = json!("other-earn-reference");
        responses[2].1["result"]["ledger"]["other-position-leg"] = positive;
        let (p, _) = profile(responses);
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "fixture-request".into(),
            data: json!({}),
        };
        let r = p
            .reconcile_earn(&account(), earn_binding(), &s, &json!({"result":true}))
            .await
            .unwrap();
        assert_eq!(r.status, Status::Unknown);
        assert!(r.verified.is_none());
    }
    #[tokio::test]
    async fn over_changed_strategy_is_not_attributed_to_original_earn() {
        let mut responses = earn_replies();
        responses[1].1["result"]["items"][0]["amount_allocated"]["total"]["native"] = json!("1.2");
        let (p, _) = profile(responses);
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "fixture-request".into(),
            data: json!({}),
        };
        assert_eq!(
            p.reconcile_earn(&account(), earn_binding(), &s, &json!({"result":true}))
                .await
                .unwrap()
                .status,
            Status::Unknown
        );
    }
    #[tokio::test]
    async fn earn_deallocation_proves_position_debit_and_withheld_cash_fee() {
        let mut b = earn_binding();
        b.allocate = false;
        let replies = vec![
            (
                E::KrakenPrivatePostEarnDeallocateStatus,
                json!({"result":{"pending":false}}),
            ),
            (
                E::KrakenPrivatePostEarnAllocations,
                json!({"result":{"items":[{"strategy_id":"fixture-strategy","amount_allocated":{"total":{"native":"0.899"}}}]}}),
            ),
            (
                E::KrakenPrivatePostLedgers,
                json!({"result":{"ledger":{"position-debit":{"type":"transfer","refid":"deallocate-original","asset":"XETH.B","amount":"-0.1","fee":"0.001"},"cash-credit":{"type":"transfer","refid":"deallocate-original","asset":"XETH","amount":"0.1","fee":"0.001"}}}}),
            ),
        ];
        let (p, _) = profile(replies);
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "original-request".into(),
            data: json!({}),
        };
        let r = p
            .reconcile_earn(&account(), b, &s, &json!({"result":true}))
            .await
            .unwrap();
        assert_eq!(r.status, Status::Completed);
        let effects = r.verified.unwrap().effects;
        assert!(effects.iter().any(|effect|matches!(effect,FinancialEffect::PositionChange{delta,..}if delta==&SignedQuantity::negative(&Quantity::from_decimal("0.101",36).unwrap()))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Cost {
                treatment: CostTreatment::DeductedFromCredit,
                ..
            }
        )));
    }
    #[tokio::test]
    async fn vanished_other_earn_strategy_invalidates_exhaustive_allocation_baseline() {
        let mut b = earn_binding();
        b.allocations_before["result"]["items"]
            .as_array_mut()
            .unwrap()
            .push(
                json!({"strategy_id":"other-strategy","amount_allocated":{"total":{"native":"2"}}}),
            );
        let (p, _) = profile(earn_replies());
        let s = Submission {
            kind: "kraken-rest-v1".into(),
            reference: "original-request".into(),
            data: json!({}),
        };
        assert_eq!(
            p.reconcile_earn(&account(), b, &s, &json!({"result":true}))
                .await
                .unwrap()
                .status,
            Status::Unknown
        );
    }
    #[tokio::test]
    async fn claimed_address_generation_checks_limited_and_missing_availability_metadata() {
        for status in ["limited", "unsupported", "missing"] {
            let generation = if status == "missing" {
                json!({})
            } else {
                json!({"status":status,"limit":1})
            };
            let mut rows = vec![
                (
                    E::KrakenFundingGetFundingV1MethodsDirection,
                    json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","asset":{"class":"currency","name":"XETH"},"deposit":{"address_generation":generation}}]}),
                ),
                (E::KrakenPublicGetAssets, assets()),
            ];
            if status == "limited" {
                rows.push((E::KrakenFundingGetFundingV2DepositAddresses,json!({"addresses":[{"id":"already-claimed","method_id":"00000000-0000-0000-0000-000000000001"}]})));
            }
            let (p, f) = profile(rows);
            let error = p
                .prepare_claim(
                    &account(),
                    E::KrakenFundingPutFundingV1DepositAddress,
                    &json!({"method_id":"00000000-0000-0000-0000-000000000001"}),
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                if status == "limited" {
                    "KRAKEN_ADDRESS_GENERATION_LIMIT"
                } else {
                    "KRAKEN_ADDRESS_GENERATION_UNSUPPORTED"
                }
            );
            assert!(f.replies.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn explicit_false_legacy_setup_fee_supports_address_generation() {
        let (p, _) = profile(vec![
            (
                E::KrakenPrivatePostDepositMethods,
                json!({"result":[{"method":"Ether (Hex)","gen-address":true,"address-setup-fee":false}]}),
            ),
            (
                E::KrakenPublicGetAssets,
                json!({"result":{"XETH":{"decimals":18}}}),
            ),
        ]);
        let b = p
            .prepare_claim(
                &account(),
                E::KrakenLegacyCreateDepositAddress,
                &json!({"asset":"XETH","method":"Ether (Hex)","new":true}),
            )
            .await
            .unwrap();
        assert!(b.fee.is_zero());
        assert!(b.current_method_id.is_none());
    }
    struct UnitPrices;
    #[async_trait]
    impl fin_pricing::UsdPriceSource for UnitPrices {
        async fn quote(&self, asset: &AssetId) -> Result<fin_pricing::UsdQuote> {
            assert!(asset.network.starts_with("kraken:tokenized_asset:"));
            assert_eq!(asset.reference, "SPYX.T");
            Ok(fin_pricing::UsdQuote {
                usd_per_unit: "1".into(),
                updated_at: now(),
                observed_at: now(),
                source: format!("synthetic-exact-unit:{}", asset.network),
            })
        }
    }
    fn tokenized_replies() -> Vec<(E, Value)> {
        let mut values = replies(json!({"method_id":"00000000-0000-0000-0000-000000000001"}));
        values[0].1["methods"][0]["asset"] = json!({"class":"tokenized_asset","name":"SPYX"});
        for field in ["fee", "gross_amount", "net_amount"] {
            values[2].1[field]["asset"] = json!({"class":"tokenized_asset","name":"SPYX"});
        }
        let metadata =
            json!({"result":{"SPYX.T":{"altname":"SPYX","aclass":"tokenized_asset","decimals":8}}});
        values[3].1 = metadata.clone();
        values[5].1 = metadata;
        values[4].1["withdrawal_limits"][0]["maximum_amount"] =
            json!({"asset":{"class":"currency","name":"USD"},"amount":"100"});
        values
    }
    #[tokio::test]
    async fn tokenized_base_and_rebased_have_distinct_approved_identities_and_explicit_wire_units()
    {
        for unit in ["base", "rebased"] {
            let (p, f) = profile(tokenized_replies());
            let p = p.with_prices(Arc::new(UnitPrices));
            let mut parameters = params();
            parameters["amount"]["asset_amount"]["asset"] =
                json!({"class":"tokenized_asset","name":"SPYX"});
            parameters["amount"]["rebase_multiplier"] = json!(unit);
            let binding = p
                .prepare_withdrawal(
                    &account(),
                    E::KrakenFundingPostFundingV1Withdrawals,
                    &mut parameters,
                    &json!({"verificationRequirement":"venue_only"}),
                )
                .await
                .unwrap();
            assert_eq!(
                binding.asset.network,
                format!("kraken:tokenized_asset:{unit}")
            );
            assert_eq!(binding.asset.reference, "SPYX.T");
            assert_eq!(binding.venue_asset_code, "SPYX");
            assert_eq!(
                parameters["fee"]["current_fee"]["max_fee"]["rebase_multiplier"],
                unit
            );
            let requests = f.requests.lock().unwrap();
            let fee = &requests
                .iter()
                .find(|(e, _)| *e == E::KrakenFundingGetFundingV1FeesMethodId)
                .unwrap()
                .1;
            assert_eq!(fee["rebase_multiplier"], unit);
            let limit = &requests
                .iter()
                .find(|(e, _)| *e == E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset)
                .unwrap()
                .1;
            assert_eq!(limit["preferred_asset"]["name"], "USD");
        }
    }
    #[tokio::test]
    async fn unpriced_tokenized_unit_stops_at_exact_identity_pricing_without_symbol_inference() {
        let (p, _) = profile(tokenized_replies());
        let mut parameters = params();
        parameters["amount"]["asset_amount"]["asset"] =
            json!({"class":"tokenized_asset","name":"SPYX"});
        parameters["amount"]["rebase_multiplier"] = json!("base");
        assert_eq!(
            p.prepare_withdrawal(
                &account(),
                E::KrakenFundingPostFundingV1Withdrawals,
                &mut parameters,
                &json!({"verificationRequirement":"venue_only"})
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_PRICE_SOURCE_UNAVAILABLE"
        );
    }
}

#[cfg(test)]
mod protected_funding_lifecycle {
    use super::*;
    use crate::{KrakenProduct, KrakenResponse, KrakenTransport};
    use fin_domain::{Adapter, SubmissionJournal};
    use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
    use std::sync::{Arc, Mutex};
    struct Journal {
        events: Arc<Mutex<Vec<&'static str>>>,
        submission: Mutex<Option<Submission>>,
        dispatch: Mutex<Option<Value>>,
    }
    impl SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, s: &Submission) -> Result<()> {
            self.events.lock().unwrap().push("persist");
            *self.submission.lock().unwrap() = Some(s.clone());
            Ok(())
        }
        fn persist_dispatch_evidence(&self, _: &Submission, e: &Value) -> Result<()> {
            self.events.lock().unwrap().push("receipt");
            *self.dispatch.lock().unwrap() = Some(e.clone());
            Ok(())
        }
    }
    struct Transport {
        events: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait]
    impl KrakenTransport for Transport {
        async fn request(
            &self,
            e: E,
            _: Option<&KrakenAccount>,
            parameters: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            let data = match e {
                E::KrakenPrivatePostListWalletAccounts => {
                    assert_eq!(parameters, json!({}));
                    json!({"result":{"accounts":[{"account_id":"WDEFAULT","status":"active","flags":{"active":true,"user_defined":false},"type":"main"},{"account_id":"WSELECTED","status":"active","flags":{"active":true,"user_defined":true},"type":"spot"}],"cursor":{"next":null}}})
                }
                E::KrakenPrivatePostGetApiKeyInfo => {
                    json!({"result":{"permissions":["query-funds","add-funds"]}})
                }
                E::KrakenFundingGetFundingV1MethodsDirection => {
                    json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","asset":{"class":"currency","name":"XETH"},"deposit":{"address_generation":{"status":"unlimited"}}}]})
                }
                E::KrakenPublicGetAssets => json!({"result":{"XETH":{"decimals":18}}}),
                E::KrakenFundingGetFundingV2DepositAddresses => json!({"addresses":[]}),
                E::KrakenFundingPutFundingV1DepositAddress => {
                    assert!(guard.unwrap()());
                    let mut events = self.events.lock().unwrap();
                    assert_eq!(events.last(), Some(&"persist"));
                    events.push("dispatch");
                    json!({"address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}})
                }
                _ => panic!("unplanned lifecycle request {e:?}"),
            };
            Ok(KrakenResponse::Json(data))
        }
    }
    #[tokio::test]
    async fn shared_adapter_persists_scope_and_response_before_reconciling_original_claim() {
        claim_lifecycle(false).await;
    }
    #[tokio::test]
    async fn shared_adapter_persists_selected_wallet_and_scoped_ledger_identity() {
        claim_lifecycle(true).await;
    }
    async fn claim_lifecycle(selected: bool) {
        let events = Arc::new(Mutex::new(vec![]));
        let account = KrakenAccount {
            name: "fixture-spot".into(),
            resource: ResourceId("kraken:fixture-only".into()),
            credential_ref: "fixture-only-credential".into(),
            physical_identity: "fixture-uid".into(),
            wallet_account_id: None,
            default_wallet_account_id: selected.then(|| "WDEFAULT".into()),
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        let profile = KrakenProfile::new(
            KrakenProduct::Spot,
            vec![account],
            Arc::new(Transport {
                events: events.clone(),
            }),
        )
        .unwrap();
        let adapter = ExchangeAdapter::new(profile);
        let mut input = json!({"account":"fixture-spot","requestId":"claim-fixture-001","method_id":"00000000-0000-0000-0000-000000000001"});
        if selected {
            input["account_id"] = json!("WSELECTED");
        }
        let p = adapter
            .prepare(
                E::KrakenFundingPutFundingV1DepositAddress.operation(),
                &input,
            )
            .await
            .unwrap();
        assert_eq!(p.binding["physicalIdentity"], "fixture-uid");
        assert!(p.intent.debits.is_empty());
        if selected {
            assert_eq!(p.binding["walletAccountId"], "WSELECTED");
            assert_eq!(
                p.binding["walletResource"],
                "kraken:fixture-only:wallet:WSELECTED"
            );
            assert_eq!(p.binding["parameters"]["account_id"], "WSELECTED");
            assert!(
                p.intent
                    .locks
                    .contains(&ResourceId("kraken:fixture-only".into()))
            );
            assert!(
                p.intent
                    .locks
                    .contains(&ResourceId("kraken:fixture-only:wallet:WSELECTED".into()))
            );
        }
        let journal = Arc::new(Journal {
            events: events.clone(),
            submission: Mutex::new(None),
            dispatch: Mutex::new(None),
        });
        let s = adapter.submit(&p, journal.clone()).await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["persist", "dispatch", "receipt"]
        );
        assert_eq!(s.data["binding"], p.binding);
        let record = OperationRecord {
            id: "fixture-operation".into(),
            request_id: "claim-fixture-001".into(),
            fingerprint: "fixture-hash".into(),
            fingerprint_version: 1,
            operation: p.operation.clone(),
            input,
            status: Status::Pending,
            created_at: now(),
            updated_at: now(),
            preview: Some(p),
            approval: None,
            submission: Some(s),
            dispatch_evidence: journal.dispatch.lock().unwrap().clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let result = adapter.reconcile(&record).await.unwrap();
        assert_eq!(result.status, Status::Completed);
        assert!(matches!(
            result.verified.unwrap().effects.as_slice(),
            [FinancialEffect::AuthorityChange { .. }]
        ));
        assert_eq!(
            *events.lock().unwrap(),
            vec!["persist", "dispatch", "receipt"]
        );
    }
}

#[path = "overrun_tests.rs"]
mod overrun_tests;
