//! Explicit owned-fork swap verification. Never loads development or production keys.
use async_trait::async_trait;
use fin_domain::*;
use fin_engine::{Engine, EngineOptions};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation};
use fin_evm::*;
use fin_store::Store;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

// Public SHA256 of "Fin owned AMM fork EOA fixture v1 - PUBLIC SYNTHETIC NEVER FUND".
// Never use or fund outside the guarded disposable fork.
const PUBLIC_ANVIL_KEY: &str = "476361fd04a5ecc99b558a630122e8dd897f82766ddf5a4c59a8a5230d4f91bf";
struct OwnedRpc {
    url: String,
    instance: String,
    chain: u64,
    block: u64,
    hash: String,
    client: reqwest::Client,
}
impl OwnedRpc {
    async fn raw(&self, method: &str, params: Value) -> Result<Value> {
        let value: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| FinError::validation("Owned fork RPC failed"))?
            .json()
            .await
            .map_err(|_| FinError::validation("Invalid fork response"))?;
        if value.get("error").is_some() {
            println!("{}", json!({"stage":"rpc_error", "method":method}));
            if method == "eth_call" {
                println!(
                    "{}",
                    json!({"stage":"synthetic_failed_call", "params":params})
                );
            }
            return Err(FinError::new(
                "FORK_RPC_ERROR",
                "fork",
                value["error"].to_string(),
                "Inspect the owned fork; do not use a remote fallback.",
            ));
        }
        if method == "eth_maxPriorityFeePerGas" {
            println!(
                "{}",
                json!({"stage":"fork_fee_bid", "value":value["result"]})
            );
        }
        if method == "eth_getTransactionReceipt" && !value["result"].is_null() {
            println!(
                "{}",
                json!({"stage":"synthetic_receipt", "receipt":value["result"]})
            );
        }
        Ok(value["result"].clone())
    }
    async fn guard(&self) -> Result<()> {
        let metadata = self.raw("anvil_metadata", json!([])).await?;
        if metadata["instanceId"] != self.instance
            || metadata["chainId"] != self.chain
            || metadata["forkedNetwork"]["forkBlockNumber"] != self.block
            || metadata["forkedNetwork"]["forkBlockHash"] != self.hash
        {
            return Err(FinError::validation(
                "Owned fork identity or immutable anchor changed",
            ));
        }
        Ok(())
    }
}
#[async_trait]
impl EvmRpc for OwnedRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.guard().await?;
        if EthereumConfig::is_reviewed_sequencer_rollup(self.chain)
            && method == "eth_getBlockByNumber"
            && params[0] == "finalized"
        {
            return Err(FinError::validation(
                "Rollup settlement must not query finalized",
            ));
        }
        self.raw(method, params).await
    }
}
struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        println!("{}", json!({"stage":"synthetic_authorization"}));
        Ok(ApprovalAudit {
            mechanism: "owned-fork-synthetic".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct FixtureValuation;
struct FixtureSigner(LocalEnvelopeSigner);
#[async_trait]
impl EnvelopeSigner for FixtureSigner {
    async fn sign_prepared(&self, tx: &FrozenTransaction) -> Result<alloy::primitives::Bytes> {
        self.0.sign_prepared(tx).await
    }
    async fn sign_authorized_permit(
        &self,
        permit: &AuthorizedPermit,
    ) -> Result<alloy::primitives::Bytes> {
        println!(
            "{}",
            json!({"stage":"synthetic_permit_signing","permit":permit,"digest":format!("{:#x}",permit.digest()?)})
        );
        self.0.sign_authorized_permit(permit).await
    }
}
#[async_trait]
impl WalletValuation for FixtureValuation {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let usd = if asset.reference == "native" {
            units.mul_div_ceil(
                &Quantity::from_u64(2_500_000_000),
                &Quantity::new("1000000000000000000")?,
            )?
        } else {
            units.mul_div_ceil(
                &Quantity::from_u64(1_000_000),
                &Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?,
            )?
        };
        Ok((
            usd,
            "Synthetic exact-identity USD fixture; not live pricing evidence".into(),
        ))
    }
}
async fn settled(
    engine: &Engine,
    rpc: &OwnedRpc,
    operation: &str,
    mut input: Value,
) -> Result<OperationRecord> {
    let authority = input
        .as_object_mut()
        .unwrap()
        .remove("_fork_authority")
        .and_then(|v| v.as_str().map(str::to_owned));
    let plan = input.as_object_mut().unwrap().remove("_fork_plan");
    let request = OperationRequest {
        request_id: input["request_id"].as_str().unwrap().into(),
        operation: operation.into(),
        input,
        run_id: None,
    };
    println!(
        "{}",
        json!({"stage":"execute", "operation":operation,"request_id":request.request_id})
    );
    let execute = async {
        if let Some(plan) = &plan {
            return engine
                .execute_financial_plan_step(
                    plan["id"].as_str().unwrap(),
                    "reverse",
                    request.clone(),
                )
                .await;
        }
        match &authority {
            Some(id) => engine.execute_boundary(id, request.clone()).await,
            None => engine.execute(request.clone()).await,
        }
    };
    let mut record = match execute.await {
        Ok(record) => record,
        Err(error) => {
            println!(
                "{}",
                json!({"stage":"synthetic_engine_error","records":engine.operations()?})
            );
            return Err(error);
        }
    };
    for _ in 0..8 {
        if record.status.terminal() {
            break;
        }
        rpc.request("anvil_mine", json!(["0x2"])).await?;
        record = engine.status(&record.id, true).await?;
    }
    if record.status != Status::Completed {
        return Err(record
            .error
            .clone()
            .unwrap_or_else(|| FinError::validation("Fork operation did not settle")));
    }
    assert!(record.effects.is_some());
    if EthereumConfig::is_reviewed_sequencer_rollup(rpc.chain) {
        assert_eq!(
            record.effects.as_ref().unwrap().evidence["finality"],
            "sequencer-inclusion"
        );
    }
    let duplicate = if let Some(plan) = &plan {
        engine
            .execute_financial_plan_step(plan["id"].as_str().unwrap(), "reverse", request)
            .await?
    } else {
        match &authority {
            Some(id) => engine.execute_boundary(id, request).await?,
            None => engine.execute(request).await?,
        }
    };
    assert_eq!(duplicate.id, record.id);
    assert_eq!(duplicate.submission, record.submission);
    Ok(record)
}
fn received(record: &OperationRecord) -> Result<Amount> {
    record
        .effects
        .as_ref()
        .and_then(|bundle| {
            bundle.effects.iter().find_map(|effect| {
                if let FinancialEffect::Conversion { received, .. } = effect {
                    Some(received.clone())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| FinError::validation("Missing verified swap conversion"))
}

#[tokio::test]
#[ignore = "Requires scripts/verify-amm-fork.py owned loopback Anvil"]
async fn native_swaps_v3_and_v4_restore_assets_and_permissions() -> Result<()> {
    let url = std::env::var("FIN_AMM_FORK_URL")
        .map_err(|_| FinError::validation("Owned fork runner required"))?;
    let port = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| FinError::validation("Exact loopback fork URL required"))?;
    assert_eq!(url, format!("http://127.0.0.1:{port}"));
    let fixture: Value = serde_json::from_str(
        &std::env::var("FIN_AMM_FORK_FIXTURE")
            .map_err(|_| FinError::validation("Fork fixture required"))?,
    )
    .map_err(|_| FinError::validation("Invalid fork fixture"))?;
    let chain = fixture["chain_id"].as_u64().unwrap();
    let dex = fixture["dex"].as_str().unwrap_or("uniswap");
    assert!(matches!(dex, "uniswap" | "ramses"));
    let quote_operation = format!("{dex}.quote");
    let swap_operation = format!("{dex}.swap");
    let rpc = Arc::new(OwnedRpc {
        url,
        instance: std::env::var("FIN_AMM_FORK_INSTANCE").unwrap(),
        chain,
        block: fixture["block"].as_u64().unwrap(),
        hash: fixture["hash"].as_str().unwrap().into(),
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
    });
    rpc.guard().await?;
    let local_signer = LocalEnvelopeSigner::from_private_key(PUBLIC_ANVIL_KEY)?;
    let owner = lower_address(local_signer.address());
    let signer = Arc::new(FixtureSigner(local_signer));
    let token = fixture["token"].as_str().unwrap();
    let mut config = EthereumConfig::wallet_only(chain, 1, vec![token.into()]);
    // Match mainnet confirmation policy; reviewed rollups use sequencer inclusion.
    config.require_finalized = chain == 1 && EthereumConfig::default().require_finalized;
    if dex == "ramses" {
        config.ramses = Some(serde_json::from_value(fixture["ramses"].clone()).unwrap());
        assert!(
            config.uniswap.is_none(),
            "Ramses must work without Uniswap configured"
        );
    } else {
        config.uniswap = Some(serde_json::from_value(fixture["uniswap"].clone()).unwrap());
    }
    let adapter = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "fork-wallet".into(),
                address: owner.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc.clone(), chain, Some(signer))),
        )?
        .with_wallet_valuation(Arc::new(FixtureValuation)),
    );
    // Test-only funding control, guarded by node ownership; no external broadcast.
    rpc.request("anvil_setBalance", json!([owner, "0x6a94d74f430000"]))
        .await?; // 0.03 ETH, $75 synthetic allocation; never external funds.
    let directory = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        Store::open(directory.path().join("operations.sqlite")).unwrap(),
        vec![adapter.clone()],
        Arc::new(Approval),
        EngineOptions::default(),
    )?;
    let base = json!({"wallet":"fork-wallet","network":"evm","chain_id":chain});
    let baseline = adapter.read("wallet.balances", &base).await?;
    let mut results = vec![];
    let protocols = if dex == "ramses" {
        vec!["v3"]
    } else {
        vec!["v3", "v4"]
    };
    for protocol in protocols {
        let mut quote = base.clone();
        quote.as_object_mut().unwrap().extend(
            json!({"token_in":"native","token_out":token,"amount":"0.001","protocol":protocol})
                .as_object()
                .unwrap()
                .clone(),
        );
        if dex == "ramses" {
            quote.as_object_mut().unwrap().remove("protocol");
            quote["tick_spacing"] = fixture["tick_spacing"].clone();
        }
        if protocol == "v4" {
            quote["v4_pool_key"] = fixture["v4_pool_key"].clone();
        }
        let q = adapter.read(&quote_operation, &quote).await?;
        let out: Quantity = serde_json::from_value(q["data"]["output"].clone()).unwrap();
        assert!(!out.is_zero());
        let mut input = quote.clone();
        input["request_id"] = json!(format!("{protocol}-forward"));
        input["recipient"] = json!(owner);
        input["deadline"] = json!(chrono::Utc::now().timestamp() as u64 + 120);
        input["max_fee_eth"] = json!("0.01");
        input["slippage_bps"] = json!(50);
        input["max_price_impact_bps"] = json!(100);
        input["min_output"] = json!(
            out.mul_div_ceil(&Quantity::from_u64(9950), &Quantity::from_u64(10000))?
                .to_decimal(6)
        );
        let grant = engine.authorize_boundary(fin_domain::boundaries::BoundaryAuthorizationRequest {
            request_id: format!("{protocol}-forward-grant"), operation: swap_operation.clone(), input: input.clone(),
            bounds: json!({"max_input":"0.001","max_fee_eth":"0.01","max_slippage_bps":100,"max_price_impact_bps":100,"minimum_rate":{"output_atoms":out.mul_div_ceil(&Quantity::from_u64(98), &Quantity::from_u64(100))?,"input_atoms":"1000000000000000"},"allowed_fee_tiers":if protocol=="v3" && dex=="uniswap" {vec![100,500,3000,10000]} else {vec![]},"max_deadline_seconds":600}),
            valid_for_seconds:600,replaces:None,
        }).await?;
        if dex == "ramses" {
            let nonce_before = rpc
                .request("eth_getTransactionCount", json!([owner, "pending"]))
                .await?;
            for (field, value) in [
                ("amount", json!("0.0011")),
                ("slippage_bps", json!(101)),
                ("max_price_impact_bps", json!(101)),
                ("max_fee_eth", json!("0.011")),
                ("max_pool_fee_ppm", json!(200)),
                ("tick_spacing", json!(5)),
                (
                    "recipient",
                    json!("0x0000000000000000000000000000000000000001"),
                ),
            ] {
                let mut changed = input.clone();
                changed[field] = value;
                assert!(
                    engine
                        .preview_boundary(&grant.id, &swap_operation, &changed)
                        .await
                        .is_err(),
                    "Ramses scope accepted changed {field}"
                );
            }
            assert!(
                engine
                    .preview_boundary(&grant.id, "uniswap.swap", &input)
                    .await
                    .is_err()
            );
            let nonce_after = rpc
                .request("eth_getTransactionCount", json!([owner, "pending"]))
                .await?;
            assert_eq!(
                nonce_before, nonce_after,
                "Rejected previews must never submit"
            );
            println!(
                "{}",
                json!({"stage":"ramses_bound_rejections", "checked":["amount","slippage","impact","gas_cap","quote_fee_ceiling","spacing","recipient","dex"],"submitted":false})
            );
        }
        input["_fork_authority"] = json!(grant.id);
        let forward = settled(&engine, &rpc, &swap_operation, input).await?;
        if dex == "ramses" {
            engine.revoke_boundary(&grant.id).await?;
            assert!(
                engine
                    .preview_boundary(&grant.id, &swap_operation, &quote)
                    .await
                    .is_err()
            );
        }
        let proceeds = received(&forward)?;
        assert_eq!(proceeds.asset.reference, token);
        let spender = if protocol == "v4" {
            "0x000000000022d473030f116ddee9f6b43ac78ba3"
        } else {
            fixture[dex]["router"].as_str().unwrap()
        };
        let mut allowance = base.clone();
        allowance["token"] = json!(token);
        allowance["spender"] = json!(spender);
        let before = adapter.read("wallet.allowance", &allowance).await?;
        let mut permit2_scope = base.clone();
        permit2_scope["token"] = json!(token);
        permit2_scope["spender"] = fixture["uniswap"]["v4"]["router"].clone();
        let permit2_before = if protocol == "v4" {
            Some(
                adapter
                    .read("wallet.permit2.allowance", &permit2_scope)
                    .await?,
            )
        } else {
            None
        };
        let old: Quantity = serde_json::from_value(before["data"]["units"].clone()).unwrap();
        if protocol == "v4" {
            let mut approve = allowance.clone();
            approve["request_id"] = json!(format!("{protocol}-allowance"));
            approve["amount"] = json!(proceeds.atoms.to_decimal(proceeds.asset.decimals));
            approve["max_fee_eth"] = json!("0.01");
            settled(&engine, &rpc, "wallet.approve", approve).await?;
        }
        let mut reverse = quote.clone();
        reverse["token_in"] = json!(token);
        reverse["token_out"] = json!("native");
        reverse["amount"] = json!(proceeds.atoms.to_decimal(proceeds.asset.decimals));
        let q = adapter.read(&quote_operation, &reverse).await?;
        let out: Quantity = serde_json::from_value(q["data"]["output"].clone()).unwrap();
        reverse["request_id"] = json!(format!("{protocol}-reverse"));
        reverse["recipient"] = json!(owner);
        reverse["deadline"] = json!(chrono::Utc::now().timestamp() as u64 + 120);
        reverse["max_fee_eth"] = json!("0.01");
        reverse["slippage_bps"] = json!(50);
        reverse["max_price_impact_bps"] = json!(100);
        reverse["min_output"] = json!(
            out.mul_div_ceil(&Quantity::from_u64(9950), &Quantity::from_u64(10000))?
                .to_decimal(18)
        );
        if dex == "ramses" {
            let prepared = adapter.prepare(&swap_operation, &reverse).await?;
            let plan: fin_domain::financial_plans::FinancialPlanSpec = serde_json::from_value(json!({
                "version":1, "name":"Owned Ramses reverse with verified proceeds",
                "steps":[{"step_id":"reverse","operation":swap_operation,"input":reverse,
                    "permitted_variations":{"max_input":proceeds.atoms.to_decimal(proceeds.asset.decimals),"max_fee_eth":"0.01","max_slippage_bps":100,"max_price_impact_bps":100,"minimum_rate":{"output_atoms":out.mul_div_ceil(&Quantity::from_u64(98), &Quantity::from_u64(100))?,"input_atoms":proceeds.atoms},"allowed_fee_tiers":[],"max_deadline_seconds":600},
                    "completion":{"operation_succeeded":true},"funding_dependencies":[]}],
                "aggregate_bounds":{"maximum_debits":prepared.intent.debits,"maximum_costs":[{
                    "location":{"kind":"cash","resource":format!("ethereum:{chain}:{owner}")},"amount":{"asset":{"network":format!("ethereum:{chain}"),"reference":"native","decimals":18},"atoms":"10000000000000000"},"treatment":"additional_debit"}]}
            })).unwrap();
            let mut authorization_request =
                fin_domain::financial_plans::FinancialPlanAuthorizationRequest {
                    request_id: "owned-ramses-reverse-plan".into(),
                    plan,
                    valid_for_seconds: 600,
                    replaces_revision_id: None,
                    expected_review_hash: None,
                };
            let review = engine
                .preview_financial_plan(authorization_request.clone())
                .await?;
            authorization_request.expected_review_hash = Some(review.review_hash);
            let authorized = engine
                .authorize_financial_plan(authorization_request)
                .await?;
            reverse["_fork_plan"] = json!({"id":authorized.plan_id});
        }
        let returned = settled(&engine, &rpc, &swap_operation, reverse).await?;
        assert_eq!(received(&returned)?.asset.reference, "native");
        let mut restore = allowance.clone();
        restore["request_id"] = json!(format!("{protocol}-restore"));
        restore["amount"] = json!(old.to_decimal(6));
        restore["max_fee_eth"] = json!("0.01");
        settled(&engine, &rpc, "wallet.approve", restore).await?;
        let after = adapter.read("wallet.allowance", &allowance).await?;
        assert_eq!(after["data"]["units"], before["data"]["units"]);
        let mut permit2_after = Value::Null;
        if let Some(p2_before) = permit2_before {
            let mut restore_p2 = permit2_scope.clone();
            restore_p2["request_id"] = json!("v4-restore-permit2");
            let p2_amount: Quantity =
                serde_json::from_value(p2_before["data"]["amount"].clone()).unwrap();
            restore_p2["amount"] = json!(p2_amount.to_decimal(proceeds.asset.decimals));
            restore_p2["expiration"] = p2_before["data"]["expiration"].clone();
            restore_p2["max_fee_eth"] = json!("0.01");
            settled(&engine, &rpc, "wallet.permit2.approve", restore_p2).await?;
            permit2_after = adapter
                .read("wallet.permit2.allowance", &permit2_scope)
                .await?;
            assert_eq!(permit2_after["data"]["amount"], p2_before["data"]["amount"]);
            assert_eq!(
                permit2_after["data"]["nonce"].as_u64(),
                p2_before["data"]["nonce"].as_u64().map(|n| n + 1)
            );
            if p2_before["data"]["expiration"] != 0 {
                assert_eq!(
                    permit2_after["data"]["expiration"],
                    p2_before["data"]["expiration"]
                );
            } else {
                assert!(permit2_after["data"]["expiration"].as_u64().unwrap() > 0);
            }
        }
        results.push(json!({"permit2_after":permit2_after,"protocol":protocol,"forward":forward,"reverse":returned,"allowance_restored":true}));
    }
    let final_state = adapter.read("wallet.balances", &base).await?;
    let stable = |value: &Value| {
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["amount"]["asset"]["reference"] == token)
            .unwrap()["amount"]["atoms"]
            .clone()
    };
    assert_eq!(stable(&baseline), stable(&final_state));
    // Reopen the same journal and reconcile settled identities. This must not
    // repeat signing or broadcast when the process restarts.
    let records_before = engine.operations()?;
    let nonce_before = rpc
        .request("eth_getTransactionCount", json!([owner, "pending"]))
        .await?;
    drop(engine);
    let recovered = Engine::new(
        Store::open(directory.path().join("operations.sqlite")).unwrap(),
        vec![adapter],
        Arc::new(Approval),
        EngineOptions::default(),
    )?;
    for original in &records_before {
        let current = recovered.status(&original.id, true).await?;
        assert_eq!(current.id, original.id);
        assert_eq!(current.status, original.status);
        assert_eq!(current.submission, original.submission);
        assert_eq!(current.effects, original.effects);
    }
    assert_eq!(
        nonce_before,
        rpc.request("eth_getTransactionCount", json!([owner, "pending"]))
            .await?
    );
    println!(
        "{}",
        json!({"dex":dex,"chain_id":chain,"results":results,"stable_restored":true,"production_writes":0,"usd_evidence":"synthetic fixture","baseline":baseline,"final":final_state})
    );
    Ok(())
}
