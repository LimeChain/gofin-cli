//! Offline synthetic fixtures only: no credentials, network calls or financial writes.
use gofin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint,
    BinanceRecipientVerifier, BinanceRequest, BinanceTransport, CredentialHandle,
    EthereumDestination, PayoutRequest, PayoutResult, TransportError,
};
use gofin_domain::{
    Adapter, AssetId, OperationRecord, PreparedOperation, Quantity, ResourceId, Status, Submission,
    SubmissionJournal,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const EVM_ADDRESS: &str = "0xAB11111111111111111111111111111111111111";
const BTC_ADDRESS: &str = "bc1qSyntheticFixtureAddress";
const XRP_ADDRESS: &str = "rSyntheticFixtureAddress";

fn config(chains: &[u64]) -> BinanceConfig {
    BinanceConfig {
        accounts: vec![BinanceAccount {
            name: "main-fixture".into(),
            resource: ResourceId("binance:synthetic-main".into()),
            credential: CredentialHandle::new("test-only:synthetic-main").unwrap(),
            role: AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: chains
            .iter()
            .map(|chain_id| EthereumDestination {
                name: "wallet-fixture".into(),
                resource: ResourceId(format!("ethereum:{chain_id}:synthetic-wallet")),
                address: EVM_ADDRESS.into(),
                chain_id: *chain_id,
                assets: BTreeMap::from([(
                    "ETH".into(),
                    AssetId {
                        network: format!("ethereum:{chain_id}"),
                        reference: "native".into(),
                        decimals: 18,
                    },
                )]),
                confirmations: 2,
            })
            .collect(),
    }
}

fn metadata(coin: &str, network: &str) -> Value {
    json!([{
        "coin":coin, "withdrawAllEnable":true, "depositAllEnable":true,
        "networkList":[{
            "coin":coin, "network":network, "withdrawEnable":true, "depositEnable":true,
            "withdrawIntegerMultiple":"0.00000001", "withdrawFee":"0.0004",
            "withdrawMin":"0.001", "withdrawMax":"1000", "withdrawTag":false, "busy":false
        }]
    }])
}

fn input(coin: &str, network: &str, address: &str) -> Value {
    json!({
        "requestId":"synthetic-generic-withdrawal", "account":"main-fixture",
        "coin":coin, "amount":"0.01", "network":network, "address":address,
        "walletType":"SPOT", "maxFee":"0.001", "minReceiveAmount":"0.009"
    })
}

struct FixtureTransport {
    metadata: Mutex<Value>,
    requests: Mutex<Vec<BinanceRequest>>,
    events: Arc<Mutex<Vec<String>>>,
    posts: AtomicUsize,
    timeout: bool,
    history_override: Mutex<Value>,
    free_after: Mutex<String>,
}

impl FixtureTransport {
    fn new(coin: &str, network: &str) -> Arc<Self> {
        Arc::new(Self {
            metadata: Mutex::new(metadata(coin, network)),
            requests: Mutex::new(vec![]),
            events: Arc::new(Mutex::new(vec![])),
            posts: AtomicUsize::new(0),
            timeout: false,
            history_override: Mutex::new(json!({})),
            free_after: Mutex::new("0.99".into()),
        })
    }

    fn last_post(&self) -> BinanceRequest {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|request| request.endpoint == BinanceEndpoint::StandardWithdrawal)
            .unwrap()
            .clone()
    }
}

#[async_trait::async_trait]
impl BinanceTransport for FixtureTransport {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        assert_eq!(
            credential,
            &CredentialHandle::new("test-only:synthetic-main").unwrap()
        );
        request.validate().unwrap();
        self.events
            .lock()
            .unwrap()
            .push(format!("request:{:?}", request.endpoint));
        self.requests.lock().unwrap().push(request.clone());
        match request.endpoint {
            BinanceEndpoint::ApiRestrictions => {
                Ok(json!({"enableReading":true,"enableWithdrawals":true}))
            }
            BinanceEndpoint::TravelRuleRequirements => {
                Ok(json!({"questionnaireCountryCode":"NIL"}))
            }
            BinanceEndpoint::CapitalConfig => Ok(self.metadata.lock().unwrap().clone()),
            BinanceEndpoint::Account => {
                let coin = self.metadata.lock().unwrap()[0]["coin"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let free = if self.posts.load(Ordering::SeqCst) == 0 {
                    "1".to_owned()
                } else {
                    self.free_after.lock().unwrap().clone()
                };
                Ok(json!({"balances":[{"asset":coin,"free":free,"locked":"0"}]}))
            }
            BinanceEndpoint::StandardWithdrawal => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                if self.timeout {
                    Err(TransportError::Timeout)
                } else {
                    Ok(json!({"id":"synthetic-withdrawal-id"}))
                }
            }
            BinanceEndpoint::WithdrawalHistory => {
                let post = self.last_post();
                assert_eq!(request.get("withdrawOrderId"), post.get("withdrawOrderId"));
                assert_eq!(request.get("coin"), post.get("coin"));
                let mut row = json!({
                    "id":"synthetic-withdrawal-id", "coin":post.get("coin"), "amount":"0.0096",
                    "network":post.get("network"), "address":post.get("address"),
                    "addressTag":post.get("addressTag"), "transactionFee":"0.0004",
                    "txId":format!("0x{}", "a".repeat(64)), "status":6,
                    "withdrawOrderId":post.get("withdrawOrderId"), "transferType":0,"walletType":0
                });
                for (key, value) in self.history_override.lock().unwrap().as_object().unwrap() {
                    row[key] = value.clone();
                }
                Ok(json!([row]))
            }
            endpoint => panic!("unexpected synthetic withdrawal endpoint {endpoint:?}"),
        }
    }
}

struct ReceiptVerifier(Mutex<Vec<PayoutRequest>>);
#[async_trait::async_trait]
impl BinanceRecipientVerifier for ReceiptVerifier {
    async fn verify(&self, request: PayoutRequest) -> PayoutResult {
        self.0.lock().unwrap().push(request);
        PayoutResult {
            status: Status::Completed,
            amount_atoms: Some(Quantity::from_decimal("0.0096", 18).unwrap()),
            evidence: json!({"fixture":"synthetic independent EVM receipt"}),
        }
    }
}

struct WrongNetReceiptVerifier;
#[async_trait::async_trait]
impl BinanceRecipientVerifier for WrongNetReceiptVerifier {
    async fn verify(&self, _request: PayoutRequest) -> PayoutResult {
        PayoutResult {
            status: Status::Completed,
            amount_atoms: Some(Quantity::from_decimal("0.0095", 18).unwrap()),
            evidence: json!({"fixture":"synthetic unrelated net credit"}),
        }
    }
}

struct ResolvingVerifier(ReceiptVerifier);
#[async_trait::async_trait]
impl BinanceRecipientVerifier for ResolvingVerifier {
    async fn resolve_asset(
        &self,
        chain_id: u64,
        coin: &str,
        contract: Option<&str>,
    ) -> gofin_domain::Result<Option<AssetId>> {
        assert_eq!(chain_id, 42161);
        assert_eq!(coin, "ETH");
        assert!(contract.is_none_or(str::is_empty));
        Ok(Some(AssetId {
            network: "ethereum:42161".into(),
            reference: "native".into(),
            decimals: 18,
        }))
    }
    async fn verify(&self, request: PayoutRequest) -> PayoutResult {
        self.0.verify(request).await
    }
}
struct UnavailableChainVerifier;
#[async_trait::async_trait]
impl BinanceRecipientVerifier for UnavailableChainVerifier {
    fn supports_chain(&self, _chain_id: u64) -> bool {
        false
    }
    async fn verify(&self, _request: PayoutRequest) -> PayoutResult {
        panic!("an unsupported chain must never reach the observer")
    }
}

struct BitcoinVerifier(Mutex<Vec<PayoutRequest>>, &'static str);
#[async_trait::async_trait]
impl BinanceRecipientVerifier for BitcoinVerifier {
    fn supports_asset(&self, network: &str, asset: &AssetId) -> bool {
        network == "BTC"
            && asset
                == &AssetId {
                    network: "bitcoin:mainnet".into(),
                    reference: "native".into(),
                    decimals: 8,
                }
    }
    async fn resolve_route_asset(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
    ) -> gofin_domain::Result<Option<AssetId>> {
        assert_eq!(network, "BTC");
        assert_eq!(coin, "BTC");
        assert!(contract.is_none_or(str::is_empty));
        Ok(Some(AssetId {
            network: "bitcoin:mainnet".into(),
            reference: "native".into(),
            decimals: 8,
        }))
    }
    async fn verify(&self, request: PayoutRequest) -> PayoutResult {
        assert_eq!(request.network, "BTC");
        assert_eq!(request.chain_id, 0);
        assert_eq!(request.memo, None);
        assert_eq!(request.tx_hash, "a".repeat(64));
        assert_eq!(request.recipient, BTC_ADDRESS);
        assert_eq!(request.asset.network, "bitcoin:mainnet");
        assert_eq!(request.asset.decimals, 8);
        self.0.lock().unwrap().push(request);
        PayoutResult {
            status: Status::Completed,
            amount_atoms: Some(Quantity::from_decimal(self.1, 8).unwrap()),
            evidence: json!({"fixture":"synthetic independent Bitcoin receipt"}),
        }
    }
}

struct Journal {
    events: Arc<Mutex<Vec<String>>>,
    persisted: Mutex<Vec<Submission>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> gofin_domain::Result<()> {
        self.events.lock().unwrap().push("approval-check".into());
        Ok(())
    }
    fn persist_submission(&self, submission: &Submission) -> gofin_domain::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push("persist-submission".into());
        self.persisted.lock().unwrap().push(submission.clone());
        Ok(())
    }
}
fn journal(transport: &FixtureTransport) -> Arc<Journal> {
    Arc::new(Journal {
        events: transport.events.clone(),
        persisted: Mutex::new(vec![]),
    })
}
fn record(preview: PreparedOperation, submission: Submission) -> OperationRecord {
    OperationRecord {
        id: "synthetic-generic-operation".into(),
        request_id: preview.input["requestId"].as_str().unwrap().into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input: preview.input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
fn request(preview: &PreparedOperation) -> BinanceRequest {
    serde_json::from_value(preview.binding["params"].clone()).unwrap()
}

#[tokio::test]
async fn named_wallet_selects_arbitrum_and_preserves_exact_route_through_settlement() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let verifier = Arc::new(ReceiptVerifier(Mutex::new(vec![])));
    let adapter = BinanceAdapter::new(
        config(&[1, 42161]),
        transport.clone(),
        None,
        Some(verifier.clone()),
    )
    .unwrap();
    let mut raw = input("ETH", "ARBITRUM", EVM_ADDRESS);
    raw.as_object_mut().unwrap().remove("address");
    raw["destination"] = json!("wallet-fixture");
    let preview = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    let params = request(&preview);
    assert_eq!(params.get("network"), Some("ARBITRUM"));
    assert_eq!(
        params.get("address").unwrap().to_lowercase(),
        EVM_ADDRESS.to_lowercase()
    );
    assert_eq!(params.get("amount"), Some("0.01"));
    assert_eq!(params.get("walletType"), Some("0"));
    assert_eq!(preview.binding["chainId"], 42161);
    assert_eq!(preview.binding["network"], "ARBITRUM");
    assert_eq!(preview.binding["receiptMode"], "chain");
    assert_eq!(preview.binding["asset"]["network"], "ethereum:42161");
    let journal = journal(&transport);
    let submitted = adapter.submit(&preview, journal.clone()).await.unwrap();
    assert_eq!(transport.last_post(), params);
    let persisted = journal.persisted.lock().unwrap()[0].clone();
    assert_eq!(persisted.reference, preview.binding["withdrawOrderId"]);
    assert_eq!(
        persisted.data["binding"]["params"],
        serde_json::to_value(&params).unwrap()
    );
    {
        let events = transport.events.lock().unwrap();
        let persist = events
            .iter()
            .position(|event| event == "persist-submission")
            .unwrap();
        let post = events
            .iter()
            .position(|event| event == "request:StandardWithdrawal")
            .unwrap();
        assert!(
            persist < post,
            "submission identity must be durable before dispatch"
        );
    }
    transport.history_override.lock().unwrap()["address"] = json!(EVM_ADDRESS.to_lowercase());
    let record = record(preview, submitted);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(
        proof.proof_digest,
        gofin_domain::verified_effect_digest(&proof).unwrap()
    );
    {
        let calls = verifier.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].chain_id, 42161);
        assert_eq!(calls[0].asset.network, "ethereum:42161");
        assert_eq!(
            calls[0].recipient.to_lowercase(),
            EVM_ADDRESS.to_lowercase()
        );
    }
    transport.history_override.lock().unwrap()["network"] = json!("ETH");
    let mismatched_network = adapter.reconcile(&record).await.unwrap();
    assert_eq!(mismatched_network.status, Status::Unknown);
    assert!(mismatched_network.verified.is_none());
    assert_eq!(
        verifier.0.lock().unwrap().len(),
        1,
        "mismatched network never reaches the receipt observer"
    );
}

#[tokio::test]
async fn raw_evm_destination_uses_observer_resolved_asset_identity() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let verifier = Arc::new(ResolvingVerifier(ReceiptVerifier(Mutex::new(vec![]))));
    let adapter =
        BinanceAdapter::new(config(&[]), transport.clone(), None, Some(verifier.clone())).unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("ETH", "ARBITRUM", EVM_ADDRESS))
        .await
        .unwrap();
    assert_eq!(preview.binding["receiptMode"], "chain");
    assert_eq!(preview.binding["asset"]["network"], "ethereum:42161");
    assert_eq!(preview.binding["asset"]["decimals"], 18);
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert!(result.verified.is_some());
    assert_eq!(verifier.0.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unavailable_chain_observer_keeps_named_wallet_receipt_pending() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let adapter = BinanceAdapter::new(
        config(&[42161]),
        transport.clone(),
        None,
        Some(Arc::new(UnavailableChainVerifier)),
    )
    .unwrap();
    let mut raw = input("ETH", "ARBITRUM", EVM_ADDRESS);
    raw.as_object_mut().unwrap().remove("address");
    raw["destination"] = json!("wallet-fixture");
    let preview = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    assert_eq!(preview.binding["receiptMode"], "unverified");
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Pending);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn raw_bitcoin_is_submitted_but_venue_completion_does_not_prove_receipt() {
    let transport = FixtureTransport::new("BTC", "BTC");
    transport.metadata.lock().unwrap()[0]["networkList"][0]["addressRegex"] =
        json!("^bc1q[A-Za-z]+$");
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("BTC", "BTC", BTC_ADDRESS))
        .await
        .unwrap();
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    assert_eq!(transport.last_post().get("address"), Some(BTC_ADDRESS));
    assert_eq!(transport.last_post().get("network"), Some("BTC"));
    assert_eq!(preview.binding["receiptMode"], "unverified");
    assert_eq!(preview.binding["chainId"], 0);
    let record = record(preview, submitted);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    assert_eq!(result.evidence["venueState"], "completed");
    assert_eq!(result.evidence["destinationReceipt"], "unverified");
    assert!(
        result.verified.is_none(),
        "venue completion cannot settle unobserved external receipt"
    );
    transport.history_override.lock().unwrap()["address"] = json!(BTC_ADDRESS.to_lowercase());
    let changed_case = adapter.reconcile(&record).await.unwrap();
    assert_eq!(
        changed_case.status,
        Status::Unknown,
        "non-EVM addresses are case-sensitive"
    );
    assert!(changed_case.verified.is_none());
}

#[tokio::test]
async fn xrp_exact_memo_is_sent_and_history_memo_tampering_stays_unresolved() {
    let transport = FixtureTransport::new("XRP", "XRP");
    {
        let mut metadata = transport.metadata.lock().unwrap();
        metadata[0]["networkList"][0]["withdrawTag"] = json!(true);
        metadata[0]["networkList"][0]["memoRegex"] = json!("^[0-9]{1,10}$");
        metadata[0]["networkList"][0]["addressRegex"] = json!("^r[A-Za-z]+$");
    }
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let mut raw = input("XRP", "XRP", XRP_ADDRESS);
    raw["memo"] = json!("00123456");
    let preview = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    assert_eq!(request(&preview).get("addressTag"), Some("00123456"));
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    assert_eq!(transport.last_post().get("addressTag"), Some("00123456"));
    let record = record(preview, submitted);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    assert!(result.verified.is_none());
    transport.history_override.lock().unwrap()["addressTag"] = json!("123456");
    let tampered = adapter.reconcile(&record).await.unwrap();
    assert_eq!(tampered.status, Status::Unknown);
    assert!(tampered.verified.is_none());
}

#[tokio::test]
async fn advertised_route_guards_block_before_any_external_post() {
    for (case, patch, input_patch, expected) in [
        (
            "unadvertised",
            json!({"network":"UNADVERTISED"}),
            json!({}),
            "UNKNOWN_NETWORK",
        ),
        (
            "disabled",
            json!({"withdrawEnable":false}),
            json!({}),
            "WITHDRAWAL_DISABLED",
        ),
        (
            "busy",
            json!({"busy":true}),
            json!({}),
            "WITHDRAWAL_DISABLED",
        ),
        (
            "precision",
            json!({"withdrawIntegerMultiple":"0.003"}),
            json!({}),
            "INVALID_PRECISION",
        ),
        (
            "fee",
            json!({"withdrawFee":"0.00100001"}),
            json!({}),
            "FEE_BOUND_EXCEEDED",
        ),
        (
            "minimum",
            json!({"withdrawMin":"0.01000001"}),
            json!({}),
            "WITHDRAWAL_LIMIT",
        ),
        (
            "maximum",
            json!({"withdrawMax":"0.00999999"}),
            json!({}),
            "WITHDRAWAL_LIMIT",
        ),
        (
            "required-memo",
            json!({"withdrawTag":true}),
            json!({}),
            "MEMO_REQUIRED",
        ),
    ] {
        let transport = FixtureTransport::new("BTC", "BTC");
        for (key, value) in patch.as_object().unwrap() {
            transport.metadata.lock().unwrap()[0]["networkList"][0][key] = value.clone();
        }
        let mut raw = input("BTC", "BTC", BTC_ADDRESS);
        for (key, value) in input_patch.as_object().unwrap() {
            raw[key] = value.clone();
        }
        let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
        let error = adapter.prepare("binance.withdraw", &raw).await.unwrap_err();
        assert_eq!(error.code, expected, "{case}");
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0, "{case}");
    }
}

#[tokio::test]
async fn destination_and_venue_patterns_are_validated_without_submission() {
    for (case, metadata_patch, input_patch, expected) in [
        (
            "unsupported-memo",
            json!({"withdrawTag":false}),
            json!({"memo":"123"}),
            "MEMO_UNSUPPORTED",
        ),
        (
            "memo-pattern",
            json!({"withdrawTag":true,"memoRegex":"^[0-9]+$"}),
            json!({"memo":"not-a-tag"}),
            "MEMO_INVALID",
        ),
        (
            "address-pattern",
            json!({"addressRegex":"^bc1q[A-Z]+$"}),
            json!({}),
            "ADDRESS_INVALID",
        ),
        (
            "invalid-pattern",
            json!({"addressRegex":"["}),
            json!({}),
            "VENUE_RESPONSE_INVALID",
        ),
        (
            "both-destinations",
            json!({}),
            json!({"destination":"wallet-fixture"}),
            "",
        ),
        ("missing-destination", json!({}), json!({"address":""}), ""),
    ] {
        let transport = FixtureTransport::new("BTC", "BTC");
        for (key, value) in metadata_patch.as_object().unwrap() {
            transport.metadata.lock().unwrap()[0]["networkList"][0][key] = value.clone();
        }
        let adapter = BinanceAdapter::new(config(&[1]), transport.clone(), None, None).unwrap();
        let mut raw = input("BTC", "BTC", BTC_ADDRESS);
        for (key, value) in input_patch.as_object().unwrap() {
            raw[key] = value.clone();
        }
        let error = adapter
            .prepare("binance.withdraw", &raw)
            .await
            .expect_err(case);
        if expected.is_empty() {
            assert!(
                !error.code.is_empty(),
                "{case} must return a structured error"
            );
        } else {
            assert_eq!(error.code, expected, "{case}");
        }
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0, "{case}");
    }
}

#[tokio::test]
async fn approval_binding_changes_when_network_address_or_memo_changes() {
    let transport = FixtureTransport::new("XRP", "XRP");
    {
        let mut metadata = transport.metadata.lock().unwrap();
        metadata[0]["networkList"][0]["withdrawTag"] = json!(true);
        let mut second = metadata[0]["networkList"][0].clone();
        second["network"] = json!("XRPL");
        metadata[0]["networkList"]
            .as_array_mut()
            .unwrap()
            .push(second);
    }
    let adapter = BinanceAdapter::new(config(&[]), transport, None, None).unwrap();
    let mut raw = input("XRP", "XRP", XRP_ADDRESS);
    raw["memo"] = json!("00123456");
    let original = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    for (key, value) in [
        ("network", "XRPL"),
        ("address", "rOtherSyntheticFixtureAddress"),
        ("memo", "00123457"),
    ] {
        let mut changed = raw.clone();
        changed[key] = json!(value);
        let changed = adapter.prepare("binance.withdraw", &changed).await.unwrap();
        assert_ne!(
            gofin_domain::binding_hash(&original.binding).unwrap(),
            gofin_domain::binding_hash(&changed.binding).unwrap(),
            "{key}"
        );
        assert_eq!(
            original.binding["withdrawOrderId"], changed.binding["withdrawOrderId"],
            "logical request identity remains stable"
        );
    }
}

#[tokio::test]
async fn tampered_approved_route_never_reaches_external_post() {
    let transport = FixtureTransport::new("XRP", "XRP");
    transport.metadata.lock().unwrap()[0]["networkList"][0]["withdrawTag"] = json!(true);
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let mut raw = input("XRP", "XRP", XRP_ADDRESS);
    raw["memo"] = json!("00123456");
    let approved = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    for (key, replacement) in [
        ("network", "BTC"),
        ("address", "rChangedFixtureAddress"),
        ("addressTag", "00123457"),
    ] {
        let mut tampered = approved.clone();
        let params = tampered.binding["params"]["params"].as_array_mut().unwrap();
        params.iter_mut().find(|param| param["key"] == key).unwrap()["value"] = json!(replacement);
        let journal = journal(&transport);
        let error = adapter
            .submit(&tampered, journal.clone())
            .await
            .unwrap_err();
        assert_eq!(error.code, "PREVIEW_CHANGED", "{key}");
        assert!(journal.persisted.lock().unwrap().is_empty(), "{key}");
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0, "{key}");
    }
}

#[tokio::test]
async fn uncertain_raw_withdrawal_reconciles_original_identity_without_replay() {
    let transport = Arc::new(FixtureTransport {
        timeout: true,
        ..Arc::try_unwrap(FixtureTransport::new("BTC", "BTC"))
            .ok()
            .unwrap()
    });
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("BTC", "BTC", BTC_ADDRESS))
        .await
        .unwrap();
    let journal = journal(&transport);
    let error = adapter.submit(&preview, journal.clone()).await.unwrap_err();
    assert_eq!(error.code, "VENUE_TIMEOUT");
    let original = {
        let persisted = journal.persisted.lock().unwrap();
        assert_eq!(persisted.len(), 1);
        persisted[0].clone()
    };
    assert_eq!(original.reference, preview.binding["withdrawOrderId"]);
    assert!(original.data.get("venueId").is_none());
    let record = record(preview, original.clone());
    for _ in 0..2 {
        let result = adapter.reconcile(&record).await.unwrap();
        assert_eq!(result.status, Status::Pending);
        assert!(result.verified.is_none());
    }
    assert_eq!(record.submission.unwrap(), original);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn custom_bitcoin_observer_settles_exact_conserved_effects_without_adapter_network_changes() {
    let transport = FixtureTransport::new("BTC", "BTC");
    transport.history_override.lock().unwrap()["txId"] = json!("a".repeat(64));
    let verifier = Arc::new(BitcoinVerifier(Mutex::new(vec![]), "0.0096"));
    let adapter =
        BinanceAdapter::new(config(&[]), transport.clone(), None, Some(verifier.clone())).unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("BTC", "BTC", BTC_ADDRESS))
        .await
        .unwrap();
    assert_eq!(preview.binding["receiptMode"], "chain");
    assert_eq!(preview.binding["chainId"], 0);
    assert_eq!(preview.binding["asset"]["network"], "bitcoin:mainnet");
    let receipt_resource: ResourceId =
        serde_json::from_value(preview.binding["receiptResource"].clone()).unwrap();
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted.clone()))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(proof.submission, submitted);
    assert_eq!(
        proof.proof_digest,
        gofin_domain::verified_effect_digest(&proof).unwrap()
    );
    assert_eq!(proof.effects.len(), 2);
    match &proof.effects[0] {
        gofin_domain::FinancialEffect::Transfer {
            from,
            to,
            debit,
            credit,
        } => {
            assert_eq!(from, &ResourceId("binance:synthetic-main".into()));
            assert_eq!(to, &receipt_resource);
            assert_eq!(debit.asset.network, "binance");
            assert_eq!(credit.asset.network, "bitcoin:mainnet");
            assert_eq!(credit.asset.decimals, 8);
            assert_eq!(debit.atoms.to_decimal(debit.asset.decimals), "0.01");
            assert_eq!(credit.atoms.to_decimal(credit.asset.decimals), "0.0096");
        }
        other => panic!("unexpected custom observer effect {other:?}"),
    }
    match &proof.effects[1] {
        gofin_domain::FinancialEffect::Cost {
            amount, treatment, ..
        } => {
            assert_eq!(amount.atoms.to_decimal(amount.asset.decimals), "0.0004");
            assert_eq!(treatment, &gofin_domain::CostTreatment::IncludedInDebit);
        }
        other => panic!("unexpected custom observer cost {other:?}"),
    }
    assert_eq!(verifier.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn immutable_legacy_withdrawal_shortfall_fails_with_verified_partial_effects() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let verifier = Arc::new(ReceiptVerifier(Mutex::new(vec![])));
    let adapter =
        BinanceAdapter::new(config(&[1, 42161]), transport.clone(), None, Some(verifier)).unwrap();
    let mut preview = adapter
        .prepare("binance.withdraw", &input("ETH", "ARBITRUM", EVM_ADDRESS))
        .await
        .unwrap();
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();

    // Model the immutable preview saved before the fee-semantics correction:
    // requested API amount .01 plus a separate .001 fee cap, with .01 net floor.
    preview.input["minReceiveAmount"] = json!("0.01");
    preview.binding["minimumReceive"] = json!("0.01");
    preview.binding["maximumGrossDebit"] = json!("0.011");
    preview.binding["maximumReceive"] = json!("0.01");
    preview.intent.maximum_costs[0].treatment = gofin_domain::CostTreatment::AdditionalDebit;
    match &mut preview.intent.expected[0] {
        gofin_domain::EffectConstraint::Transfer { minimum_credit, .. } => {
            *minimum_credit = Quantity::from_decimal("0.01", 18).unwrap();
        }
        other => panic!("unexpected saved withdrawal constraint {other:?}"),
    }
    let mut submitted = submitted;
    submitted.data["minimumReceive"] = json!("0.01");
    submitted.data["maximumGrossDebit"] = json!("0.011");
    submitted.data["maximumReceive"] = json!("0.01");
    let pending = record(preview, submitted);

    let result = adapter.reconcile(&pending).await.unwrap();
    assert_eq!(result.status, Status::Failed);
    let proof = result
        .verified
        .expect("verified partial payout is terminal");
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(proof.evidence["actual_gross_debit"], "0.01");
    assert_eq!(proof.evidence["actual_net_receipt"], "0.0096");
    assert_eq!(proof.evidence["actual_fee"], "0.0004");
    assert_eq!(proof.evidence["approved_minimum"], "0.01");
    assert_eq!(proof.evidence["minimum_receipt_met"], false);
    assert_eq!(proof.evidence["partial_payout_reconciled"], true);
    assert!(
        proof.evidence["outcome_reason"]
            .as_str()
            .unwrap()
            .contains("do not retry")
    );
    match &proof.effects[0] {
        gofin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.atoms.to_decimal(debit.asset.decimals), "0.0096");
            assert_eq!(credit.atoms.to_decimal(credit.asset.decimals), "0.0096");
        }
        other => panic!("unexpected partial withdrawal transfer {other:?}"),
    }
    match &proof.effects[1] {
        gofin_domain::FinancialEffect::Cost {
            amount, treatment, ..
        } => {
            assert_eq!(amount.atoms.to_decimal(amount.asset.decimals), "0.0004");
            assert_eq!(treatment, &gofin_domain::CostTreatment::AdditionalDebit);
        }
        other => panic!("unexpected partial withdrawal fee {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_account_debit_does_not_change_exact_withdrawal_effect_amount() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    // Another concurrent operation debits .00384 ETH from the same shared
    // account; this withdrawal remains attributable only by its own immutable
    // venue history identity and canonical recipient receipt.
    *transport.free_after.lock().unwrap() = "0.98616".into();
    let verifier = Arc::new(ReceiptVerifier(Mutex::new(vec![])));
    let adapter =
        BinanceAdapter::new(config(&[42161]), transport.clone(), None, Some(verifier)).unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("ETH", "ARBITRUM", EVM_ADDRESS))
        .await
        .unwrap();
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(proof.evidence["apiGrossAmount"], "0.01");
    assert_eq!(proof.evidence["sourceDebitObserved"], "0.01");
    assert_eq!(proof.evidence["aggregateSourceDebitObserved"], "0.01384");
    match &proof.effects[0] {
        gofin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.atoms.to_decimal(debit.asset.decimals), "0.01");
            assert_eq!(credit.atoms.to_decimal(credit.asset.decimals), "0.0096");
        }
        other => panic!("unexpected concurrent withdrawal effect {other:?}"),
    }
}

#[tokio::test]
async fn withdrawal_with_history_identity_but_wrong_chain_amount_stays_unresolved() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let adapter = BinanceAdapter::new(
        config(&[42161]),
        transport.clone(),
        None,
        Some(Arc::new(WrongNetReceiptVerifier)),
    )
    .unwrap();
    let preview = adapter
        .prepare("binance.withdraw", &input("ETH", "ARBITRUM", EVM_ADDRESS))
        .await
        .unwrap();
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn legacy_mainnet_record_retains_original_chain_after_wallet_configuration_reordering() {
    let transport = FixtureTransport::new("ETH", "ETH");
    let verifier = Arc::new(ReceiptVerifier(Mutex::new(vec![])));
    let original = BinanceAdapter::new(
        config(&[1]),
        transport.clone(),
        None,
        Some(verifier.clone()),
    )
    .unwrap();
    let mut raw = input("ETH", "ETH", EVM_ADDRESS);
    raw.as_object_mut().unwrap().remove("address");
    raw["destination"] = json!("wallet-fixture");
    let mut preview = original.prepare("binance.withdraw", &raw).await.unwrap();
    let mut submitted = original
        .submit(&preview, journal(&transport))
        .await
        .unwrap();
    for key in [
        "receiptResource",
        "receiptAddress",
        "network",
        "memo",
        "receiptMode",
    ] {
        preview.binding.as_object_mut().unwrap().remove(key);
        submitted.data["binding"]
            .as_object_mut()
            .unwrap()
            .remove(key);
    }
    for key in ["network", "memo"] {
        submitted.data.as_object_mut().unwrap().remove(key);
    }
    for key in ["address", "memo"] {
        preview.input.as_object_mut().unwrap().remove(key);
    }
    let pending = record(preview, submitted.clone());
    let upgraded = BinanceAdapter::new(
        config(&[42161, 1]),
        transport.clone(),
        None,
        Some(verifier.clone()),
    )
    .unwrap();
    let result = upgraded.reconcile(&pending).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(
        proof.submission, submitted,
        "recovery preserves the original durable identity"
    );
    match &proof.effects[0] {
        gofin_domain::FinancialEffect::Transfer { to, credit, .. } => {
            assert_eq!(to, &ResourceId("ethereum:1:synthetic-wallet".into()));
            assert_eq!(credit.asset.network, "ethereum:1");
        }
        other => panic!("unexpected legacy withdrawal effect {other:?}"),
    }
    let calls = verifier.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].chain_id, 1);
    assert_eq!(calls[0].network, "ETH");
    assert_eq!(calls[0].memo, None);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_published_fee_and_zero_fee_cap_settle_without_an_invented_cost() {
    let transport = FixtureTransport::new("BTC", "BTC");
    transport.metadata.lock().unwrap()[0]["networkList"][0]["withdrawFee"] = json!("0");
    *transport.free_after.lock().unwrap() = "0.99".into();
    *transport.history_override.lock().unwrap() =
        json!({"amount":"0.01", "transactionFee":"0", "txId":"a".repeat(64)});
    let adapter = BinanceAdapter::new(
        config(&[]),
        transport.clone(),
        None,
        Some(Arc::new(BitcoinVerifier(Mutex::new(vec![]), "0.01"))),
    )
    .unwrap();
    let mut raw = input("BTC", "BTC", BTC_ADDRESS);
    raw["maxFee"] = json!("0");
    raw["minReceiveAmount"] = json!("0.01");
    let preview = adapter.prepare("binance.withdraw", &raw).await.unwrap();
    assert_eq!(preview.binding["maximumFee"], "0");
    assert_eq!(preview.binding["maximumGrossDebit"], "0.01");
    let submitted = adapter.submit(&preview, journal(&transport)).await.unwrap();
    let result = adapter
        .reconcile(&record(preview, submitted))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(
        proof.effects.len(),
        1,
        "zero fee produces no fabricated cost effect"
    );
    match &proof.effects[0] {
        gofin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.atoms.to_decimal(debit.asset.decimals), "0.01");
            assert_eq!(credit.atoms.to_decimal(credit.asset.decimals), "0.01");
        }
        other => panic!("unexpected zero fee effect {other:?}"),
    }
}

#[tokio::test]
async fn fee_cap_larger_than_gross_api_amount_is_rejected_before_posting() {
    let transport = FixtureTransport::new("BTC", "BTC");
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let mut raw = input("BTC", "BTC", BTC_ADDRESS);
    raw["maxFee"] = json!("0.01000001");
    let error = adapter.prepare("binance.withdraw", &raw).await.unwrap_err();
    assert_eq!(error.code, "WITHDRAWAL_FEE_EXCEEDS_AMOUNT");
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn external_evm_resource_normalizes_address_casing_and_empty_memo_identity() {
    let transport = FixtureTransport::new("ETH", "ARBITRUM");
    let adapter = BinanceAdapter::new(config(&[]), transport.clone(), None, None).unwrap();
    let original = adapter
        .prepare("binance.withdraw", &input("ETH", "ARBITRUM", EVM_ADDRESS))
        .await
        .unwrap();
    let mut equivalent = input("ETH", "ARBITRUM", &EVM_ADDRESS.to_lowercase());
    equivalent["memo"] = json!("");
    let equivalent = adapter
        .prepare("binance.withdraw", &equivalent)
        .await
        .unwrap();
    assert_eq!(
        original.binding["receiptResource"],
        equivalent.binding["receiptResource"]
    );
    assert_eq!(
        original.intent.locks, equivalent.intent.locks,
        "equivalent destinations share accounting and reservation identity"
    );
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}
