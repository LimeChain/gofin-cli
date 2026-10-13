//! Protected composition of a reviewed EVM transfer and exact Kraken usable
//! credit. Address discovery never claims/generates an address as a side effect.
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, Check, CheckStatus, CostBound, CostTreatment,
    Definition, EffectConstraint, FinError, FinancialEffect, FinancialIntent, FinancialState,
    OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId,
    Result, RevalidatedOperation, StateSnapshot, Status, Submission, SubmissionJournal,
    binding_hash, now, verified_effect_digest,
};
use fin_exchange::ExchangeProfile;
use fin_kraken::funding::{
    KrakenDepositInput as DepositInput, deposit_definition, native_quantity,
};
use fin_kraken::{KrakenEndpoint as E, KrakenProfile};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
fn error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "kraken_deposit",
        message,
        "Inspect the original deposit status; never repeat an uncertain source transaction.",
    )
}
fn raw(value: &Value) -> Result<&Value> {
    let v = &value["data"];
    if v.get("error")
        .and_then(Value::as_array)
        .is_some_and(|e| !e.is_empty())
    {
        return Err(error(
            "KRAKEN_DEPOSIT_EVIDENCE_INVALID",
            "Kraken deposit evidence reports an error.",
        ));
    }
    Ok(v.get("result").unwrap_or(v))
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "KRAKEN_DEPOSIT_EVIDENCE_INVALID",
                "Exact deposit evidence is missing.",
            )
        })
}
fn unknown(reason: &str, detail: Value) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"detail":detail,"resubmissionAllowed":false}),
        verified: None,
    }
}
fn discovery_api() -> String {
    "funding-v1-v2".into()
}
fn credit_api() -> String {
    "legacy-v0".into()
}
fn deposit_status_method_matches(selected: &str, reported: &str) -> bool {
    reported == selected
        || reported
            .strip_suffix(" (Unified)")
            .is_some_and(|base| base == selected)
}
fn deposit_route_digest(binding: &DepositBinding) -> Result<String> {
    // Kraken's route response contains floating-point presentation metadata.
    // Hash only the typed identity and exact approved quantities; re-hashing
    // the raw response would either admit binary floats or make recovery fail.
    let mut identity = json!({
        "method_id":binding.method_id,
        "method_name":binding.method_name,
        "network_id":binding.network_id,
        "network_name":binding.network_name,
        "chain_id":binding.chain_id,
        "address":binding.address,
        "venue_asset":binding.venue_asset,
        "gross":binding.gross,
        "net":binding.net,
        "fee":binding.fee,
        "fee_asset":binding.fee_asset,
    });
    // Keep the historical EVM digest stable for persisted operation proofs.
    if binding.source_kind == "solana" {
        identity["source_kind"] = json!(binding.source_kind);
        identity["cluster"] = json!(binding.cluster);
        identity["mint"] = json!(binding.mint);
        identity["source_asset"] = json!(binding.source_asset);
    }
    binding_hash(&identity)
}
fn exact_deposit_evidence(row: &Value) -> Value {
    json!({
        "aclass":row["aclass"],
        "asset":row["asset"],
        "method":row["method"],
        "refid":row["refid"],
        "txid":row["txid"],
        "info":row["info"],
        "amount":row["amount"],
        "fee":row["fee"],
        "status":row["status"],
        "status-prop":row["status-prop"],
    })
}
fn exact_ledger_evidence(identity: &str, entry: &Value) -> Value {
    // Kraken's ledger `time` is a fractional JSON number. Amounts and every
    // accounting identity are exact strings; omitting display time keeps the
    // immutable proof hash free of floats.
    json!({
        "identity":identity,
        "aclass":entry["aclass"],
        "type":entry["type"],
        "subtype":entry["subtype"],
        "refid":entry["refid"],
        "asset":entry["asset"],
        "amount":entry["amount"],
        "fee":entry["fee"],
        "balance":entry["balance"],
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DepositBinding {
    #[serde(
        default,
        rename = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    usd_ceiling: Option<Value>,
    account: String,
    #[serde(default = "discovery_api")]
    discovery_api: String,
    #[serde(default = "credit_api")]
    credit_api: String,
    account_resource: ResourceId,
    #[serde(
        default = "evm_source_kind",
        skip_serializing_if = "is_evm_source_kind"
    )]
    source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mint: Option<String>,
    #[serde(default)]
    physical_identity: Option<String>,
    method_id: String,
    method_name: String,
    network_id: String,
    network_name: String,
    chain_id: u64,
    address: String,
    #[serde(default)]
    address_id: Option<String>,
    venue_asset: AssetId,
    source_asset: AssetId,
    gross: Quantity,
    net: Quantity,
    fee: Quantity,
    #[serde(default)]
    fee_asset: Option<AssetId>,
    free_before: Quantity,
    route: Value,
    inner_input: Value,
    inner: PreparedOperation,
}
fn decode_binding(raw: &Value) -> std::result::Result<DepositBinding, serde_json::Error> {
    let mut native = raw.clone();
    if let Some(fields) = native.as_object_mut() {
        // Engine-owned authorization annotations never become venue fields.
        fields.remove(fin_domain::financial_plans::PLAN_METADATA_KEY);
        fields.remove(fin_domain::boundaries::BOUNDARY_METADATA_KEY);
    }
    serde_json::from_value(native)
}
pub(crate) struct KrakenEvmDepositAdapter {
    kraken: Arc<KrakenProfile>,
    ethereum: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
    solana: BTreeMap<String, Arc<fin_solana::SolanaWalletAdapter>>,
}
fn evm_source_kind() -> String {
    "evm".into()
}
fn is_evm_source_kind(source_kind: &String) -> bool {
    source_kind == "evm"
}
impl KrakenEvmDepositAdapter {
    pub(crate) fn with_solana(
        kraken: Arc<KrakenProfile>,
        ethereum: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
        solana: BTreeMap<String, Arc<fin_solana::SolanaWalletAdapter>>,
    ) -> Self {
        Self {
            kraken,
            ethereum,
            solana,
        }
    }
    fn input(raw: &Value) -> Result<DepositInput> {
        serde_json::from_value(raw.clone()).map_err(|_| {
            FinError::validation("Kraken deposit input is malformed or contains unknown fields.")
        })
    }
    async fn read(&self, e: E, account: &str, mut params: Value) -> Result<Value> {
        params["account"] = json!(account);
        self.kraken.read(e.operation(), &params).await
    }
    async fn free(&self, account: &str, asset: &AssetId) -> Result<Quantity> {
        let mut query = json!({});
        if let Some(unit) = asset.network.strip_prefix("kraken:tokenized_asset:") {
            query["rebase_multiplier"] = json!(unit);
        }
        let r = self
            .read(E::KrakenPrivatePostBalanceEx, account, query)
            .await?;
        let b = raw(&r)?.get(&asset.reference).ok_or_else(|| {
            error(
                "KRAKEN_DEPOSIT_BALANCE_MISSING",
                "Selected deposit asset is absent from extended balance.",
            )
        })?;
        native_quantity(text(b, "balance")?, 36)?
            .checked_sub(&native_quantity(
                b.get("hold_trade")
                    .or_else(|| b.get("hold_trading"))
                    .and_then(Value::as_str)
                    .unwrap_or("0"),
                36,
            )?)?
            .checked_sub(&native_quantity(
                b.get("credit_used").and_then(Value::as_str).unwrap_or("0"),
                36,
            )?)
    }
    async fn pages(&self, e: E, account: &str, mut p: Value, key: &str) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursors = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let response = self.read(e, account, p.clone()).await?;
            let r = raw(&response)?;
            out.extend(
                r.get(key)
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_DEPOSIT_EVIDENCE_INVALID",
                            "Deposit discovery list is malformed.",
                        )
                    })?
                    .iter()
                    .cloned(),
            );
            match r
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                None => return Ok(out),
                Some(c) if cursors.insert(c.to_owned()) => p["cursor"] = json!(c),
                _ => {
                    return Err(error(
                        "KRAKEN_DEPOSIT_COVERAGE_INCOMPLETE",
                        "Deposit pagination repeats a cursor.",
                    ));
                }
            }
        }
        Err(error(
            "KRAKEN_DEPOSIT_COVERAGE_INCOMPLETE",
            "Deposit discovery exceeds its bounded page scope.",
        ))
    }
    async fn prepare_deposit(
        &self,
        raw_input: &Value,
        original: Option<&PreparedOperation>,
    ) -> Result<PreparedOperation> {
        let input = Self::input(raw_input)?;
        let solana_source = input.network.as_deref() == Some("solana");
        if input
            .network
            .as_deref()
            .is_some_and(|network| network != "solana")
            || (solana_source
                && (input.chain_id != 0
                    || !input.max_fee_eth.is_empty()
                    || input.cluster.as_deref() != Some(fin_solana::CLUSTER)
                    || input.max_fee_sol.as_deref().is_none_or(str::is_empty)
                    || input.token.is_some()
                    || input.denomination.is_some()))
            || (!solana_source
                && (input.cluster.is_some()
                    || input.mint.is_some()
                    || input.max_fee_sol.is_some()
                    || input.max_account_funding_sol.is_some()
                    || input.chain_id == 0
                    || input.max_fee_eth.is_empty()))
        {
            return Err(FinError::validation(
                "Choose exactly one supported Kraken deposit source: reviewed EVM or Solana mainnet-beta.",
            ));
        }
        if solana_source && let Some(mint) = input.mint.as_deref() {
            fin_solana::parse_address(mint).map_err(|_| {
                FinError::validation("Solana mint must be one exact canonical base58 address.")
            })?;
        }
        if input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(FinError::validation(
                "A bounded deposit requestId is required.",
            ));
        }
        let account = self
            .kraken
            .connections()
            .into_iter()
            .find(|c| c.name == input.account && c.provider == "kraken")
            .ok_or_else(|| {
                error(
                    "KRAKEN_ACCOUNT_UNKNOWN",
                    "Deposit requires an enrolled Kraken spot account.",
                )
            })?;
        let permissions = self
            .read(E::KrakenPrivatePostGetApiKeyInfo, &input.account, json!({}))
            .await?;
        let permissions = raw(&permissions)?
            .get("permissions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                error(
                    "KRAKEN_PERMISSIONS_UNAVAILABLE",
                    "The deposit account did not return its exact readable-credit permissions.",
                )
            })?;
        for required in ["query-funds", "query-ledger"] {
            if !permissions
                .iter()
                .any(|permission| permission.as_str() == Some(required))
            {
                return Err(error(
                    "KRAKEN_PERMISSION_MISSING",
                    &format!(
                        "Wallet-funded deposits require the Kraken {required} permission to prove usable credit before settlement."
                    ),
                ));
            }
        }
        let source_adapter: Arc<dyn Adapter> = if solana_source {
            self.solana
                .get(&input.wallet)
                .cloned()
                .map(|adapter| adapter as Arc<dyn Adapter>)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_WALLET_UNKNOWN",
                        "Solana deposit source requires an enrolled mainnet-beta wallet.",
                    )
                })?
        } else {
            self.ethereum
                .get(&input.chain_id)
                .cloned()
                .map(|adapter| adapter as Arc<dyn Adapter>)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_CHAIN_UNSUPPORTED",
                        "Deposit source requires a configured reviewed EVM chain.",
                    )
                })?
        };
        let wallet = source_adapter
            .connections()
            .into_iter()
            .find(|c| c.name == input.wallet && c.signing_enabled)
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_WALLET_UNKNOWN",
                    "Deposit source requires an enrolled signing wallet on this chain.",
                )
            })?;
        let denomination = input.denomination.map(|v| v.as_str()).unwrap_or("rebased");
        let methods = self
            .pages(
                E::KrakenFundingGetFundingV1MethodsDirection,
                &input.account,
                json!({"direction":"deposit","limit":500,"rebase_multiplier":denomination}),
                "methods",
            )
            .await?;
        let selected: Vec<_> = methods
            .iter()
            .filter(|v| v["method_id"] == input.method_id)
            .collect();
        if selected.len() != 1 {
            return Err(error(
                "KRAKEN_DEPOSIT_ROUTE_AMBIGUOUS",
                "Deposit method must resolve one current authenticated route.",
            ));
        }
        let method = selected[0];
        let network_name = text(&method["network"], "network_name")?;
        if (!solana_source && crate::kraken::reviewed_chain(network_name) != Some(input.chain_id))
            || (solana_source && !crate::kraken::reviewed_solana_network(network_name))
        {
            return Err(error(
                "KRAKEN_DEPOSIT_CHAIN_MISMATCH",
                "Kraken network identity does not match the selected reviewed source chain.",
            ));
        }
        let asset_class = text(&method["asset"], "class")?;
        if !matches!(asset_class, "currency" | "tokenized_asset") {
            return Err(error(
                "KRAKEN_DEPOSIT_ASSET_UNSUPPORTED",
                "Funding method does not provide a supported native asset class.",
            ));
        }
        if asset_class == "tokenized_asset" && denomination != "base" {
            return Err(error(
                "KRAKEN_DEPOSIT_DENOMINATION_UNSUPPORTED",
                "EVM token transfers prove SPV base token quantities. Select denomination=base; rebased equity conversion needs reviewed factor evidence.",
            ));
        }
        let coin = text(&method["asset"], "name")?;
        let metadata = self
            .kraken
            .read(
                E::KrakenPublicGetAssets.operation(),
                &json!({"asset":coin,"aclass":asset_class}),
            )
            .await?;
        let currencies = raw(&metadata)?.as_object().ok_or_else(|| {
            error(
                "KRAKEN_DEPOSIT_ASSET_UNKNOWN",
                "Kraken currency metadata is malformed.",
            )
        })?;
        let exact = currencies
            .iter()
            .filter(|(id, v)| id.as_str() == coin || v["altname"].as_str() == Some(coin))
            .collect::<Vec<_>>();
        if exact.len() != 1
            || exact[0].1["decimals"].as_u64().is_none_or(|v| v > 36)
            || exact[0].1.get("aclass").is_some_and(|v| v != asset_class)
            || (asset_class == "tokenized_asset" && exact[0].1["aclass"] != "tokenized_asset")
        {
            return Err(error(
                "KRAKEN_DEPOSIT_ASSET_UNKNOWN",
                "Kraken did not establish one exact currency identity and precision.",
            ));
        }
        fin_kraken::funding::native_quantity(&input.amount,exact[0].1["decimals"].as_u64().expect("validated precision") as u8).map_err(|_|error("KRAKEN_DEPOSIT_AMOUNT_PRECISION", "Deposit amount exceeds this exact Kraken asset's published decimal precision; Fin never rounds source principal or credited quantity."))?;
        let venue_asset = AssetId {
            network: if asset_class == "currency" {
                "kraken".into()
            } else {
                format!("kraken:tokenized_asset:{denomination}")
            },
            reference: exact[0].0.clone(),
            decimals: 36,
        };
        // Funding v1 methods can share one claimed address family. The v2
        // claimed-address schema exposes method_id/address details, but no
        // address id; use the published sharing identity and bind a concrete
        // address when the family contains more than one destination.
        let address_method_id = method["deposit"]["shares_addresses_with_method_id"]
            .as_str()
            .unwrap_or(&input.method_id);
        let rows = self
            .pages(
                E::KrakenFundingGetFundingV2DepositAddresses,
                &input.account,
                json!({"scope":{"method_id":address_method_id},"limit":500}),
                "addresses",
            )
            .await?;
        let candidates: Vec<_> =
            rows.iter()
                .filter(|r| {
                    let belongs_to_family = r["method_id"] == address_method_id
                        || r["shares_addresses_with_method_id"] == address_method_id
                        || r["method_id"] == input.method_id;
                    let selected_id = input.address_id.as_ref().is_none_or(|id| r["id"] == *id);
                    let selected_address = input.address.as_ref().is_none_or(|address| {
                        r["address_details"]["crypto"]["address"] == *address
                    });
                    belongs_to_family && selected_id && selected_address
                })
                .collect();
        if candidates.len() != 1 {
            return Err(error(
                "KRAKEN_DEPOSIT_ADDRESS_REQUIRED",
                "Select one exact existing claimed deposit address when Kraken returns multiple addresses; claim an address separately when none exists.",
            ));
        }
        let address = candidates[0];
        let crypto = &address["address_details"]["crypto"];
        if ["tag", "memo"].iter().any(|k| {
            crypto
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        }) {
            return Err(error(
                "KRAKEN_DEPOSIT_MEMO_UNSUPPORTED",
                "Reviewed EVM deposits cannot carry a tag or memo.",
            ));
        }
        if let Some(expiry) = address.get("expire_time").filter(|v| !v.is_null())
            && expiry
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .is_none_or(|t| t <= Utc::now())
        {
            return Err(error(
                "KRAKEN_DEPOSIT_ADDRESS_EXPIRED",
                "Selected claimed deposit address is expired or its expiry is invalid.",
            ));
        }
        let recipient = text(crypto, "address")?;
        let contract = method["network"]
            .get("contract_address")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let source_asset = if solana_source {
            if (coin == "SOL" && input.mint.is_some())
                || (coin != "SOL" && input.mint.as_deref() != contract)
            {
                return Err(error(
                    "KRAKEN_DEPOSIT_TOKEN_MISMATCH",
                    "Solana SPL deposits require the exact route mint; native SOL must omit mint.",
                ));
            }
            let wallet = self
                .solana
                .get(&input.wallet)
                .expect("selected Solana adapter");
            let asset =
                fin_solana::funding::resolve_external_asset(&wallet.runtime(), coin, contract)
                    .await?;
            if asset.reference != "native"
                && (coin != "USDC" || asset.reference != fin_solana::funding::MAINNET_USDC_MINT)
            {
                return Err(error(
                    "KRAKEN_DEPOSIT_SOURCE_ASSET_UNSUPPORTED",
                    "Kraken Solana deposits currently support native SOL and the pinned mainnet USDC mint only.",
                ));
            }
            asset
        } else {
            use fin_binance::BinanceRecipientVerifier;
            let observer = crate::EthereumPayoutVerifier::new(self.ethereum.clone())?;
            observer
                .resolve_asset(
                    input.chain_id,
                    if coin == "XETH" { "ETH" } else { coin },
                    contract,
                )
                .await?
                .ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_SOURCE_ASSET_UNSUPPORTED",
                        "Exact Kraken network contract is not a supported observed chain asset.",
                    )
                })?
        };
        if input.token.as_ref().is_some_and(|token| {
            if solana_source {
                token != &source_asset.reference
            } else {
                !token.eq_ignore_ascii_case(&source_asset.reference)
            }
        }) {
            return Err(error(
                "KRAKEN_DEPOSIT_TOKEN_MISMATCH",
                "Selected token differs from the venue route's independently observed chain asset.",
            ));
        }
        let source_atoms = Quantity::from_decimal(&input.amount, source_asset.decimals)?;
        if source_atoms.is_zero() {
            return Err(FinError::validation("Deposit amount must be positive."));
        }
        let gross = Quantity::from_decimal(&input.amount, 36)?;
        let quote = self
            .read(
                E::KrakenFundingGetFundingV1FeesMethodId,
                &input.account,
                json!({"method_id":input.method_id,"amount":input.amount,"fee_included":true,"rebase_multiplier":denomination}),
            )
            .await?;
        let q = raw(&quote)?;
        if [&q["gross_amount"], &q["net_amount"]]
            .iter()
            .any(|v| v["asset"]["name"] != coin || v["asset"]["class"] != asset_class)
        {
            return Err(error(
                "KRAKEN_DEPOSIT_FEE_ASSET_UNSUPPORTED",
                "Deposit quote changed the exact principal asset identity.",
            ));
        }
        let fee_class = text(&q["fee"]["asset"], "class")?;
        let fee_code = text(&q["fee"]["asset"], "name")?;
        let fee_asset = if fee_class == asset_class && fee_code == coin {
            venue_asset.clone()
        } else {
            if !matches!(fee_class, "currency" | "tokenized_asset") {
                return Err(error(
                    "KRAKEN_DEPOSIT_FEE_ASSET_UNSUPPORTED",
                    "Deposit fee has an unsupported asset class.",
                ));
            }
            let response = self
                .kraken
                .read(
                    E::KrakenPublicGetAssets.operation(),
                    &json!({"asset":fee_code,"aclass":fee_class}),
                )
                .await?;
            let identities = raw(&response)?.as_object().ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_ASSET_UNKNOWN",
                    "Fee asset metadata is not an identity map.",
                )
            })?;
            let matches = identities
                .iter()
                .filter(|(id, v)| id.as_str() == fee_code || v["altname"] == fee_code)
                .collect::<Vec<_>>();
            if matches.len() != 1
                || matches[0].1["decimals"].as_u64().is_none_or(|v| v > 36)
                || matches[0].1.get("aclass").is_some_and(|v| v != fee_class)
                || (fee_class == "tokenized_asset" && matches[0].1["aclass"] != "tokenized_asset")
            {
                return Err(error(
                    "KRAKEN_DEPOSIT_ASSET_UNKNOWN",
                    "Deposit fee asset class, native identity or precision could not be independently established.",
                ));
            }
            AssetId {
                network: if fee_class == "currency" {
                    "kraken".into()
                } else {
                    format!("kraken:tokenized_asset:{denomination}")
                },
                reference: matches[0].0.clone(),
                decimals: 36,
            }
        };
        let separate_fee = fee_asset != venue_asset;
        let fee = native_quantity(text(&q["fee"], "amount")?, 36)?;
        let net = native_quantity(text(&q["net_amount"], "amount")?, 36)?;
        if input
            .max_deposit_fee
            .as_deref()
            .is_some_and(|cap| Quantity::from_decimal(cap, 36).map_or(true, |cap| fee > cap))
        {
            return Err(error(
                "KRAKEN_DEPOSIT_FEE_LIMIT",
                "Observed exact deposit fee exceeds maxDepositFee.",
            ));
        }
        if native_quantity(text(&q["gross_amount"], "amount")?, 36)? != gross
            || (if separate_fee {
                net.clone()
            } else {
                net.checked_add(&fee)?
            }) != gross
        {
            return Err(error(
                "KRAKEN_DEPOSIT_FEE_INVALID",
                "Deposit gross/net/fee arithmetic is inconsistent.",
            ));
        }
        if method
            .get("minimum_amount")
            .and_then(Value::as_str)
            .is_some_and(|s| native_quantity(s, 36).map_or(true, |v| net < v))
        {
            return Err(error(
                "KRAKEN_DEPOSIT_MINIMUM",
                "Net deposit is below this exact method's minimum.",
            ));
        }
        if method
            .get("maximum_amount")
            .and_then(Value::as_str)
            .is_some_and(|s| native_quantity(s, 36).map_or(true, |v| gross > v))
        {
            return Err(error(
                "KRAKEN_DEPOSIT_LIMIT",
                "Deposit exceeds the method's exact maximum.",
            ));
        }
        let limits = self
            .read(
                E::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset,
                &input.account,
                if asset_class=="tokenized_asset"{json!({"asset_class":asset_class,"asset":coin,"preferred_asset":{"class":"currency","name":"USD"}})}else{json!({"asset_class":asset_class,"asset":coin})},
            )
            .await?;
        let limit_rows = raw(&limits)?
            .get("deposit_limits")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_LIMIT_MISSING",
                    "Current account-specific deposit limits are missing.",
                )
            })?;
        let matching: Vec<_> = limit_rows
            .iter()
            .filter(|v| v["method_id"] == input.method_id)
            .collect();
        if matching.len() != 1 {
            return Err(error(
                "KRAKEN_DEPOSIT_LIMIT",
                "Deposit must resolve one current method/account limit.",
            ));
        }
        let has_account_maximum = !matching[0]["maximum_amount"].is_null();
        if asset_class == "tokenized_asset" {
            if !has_account_maximum {
                return Err(error(
                    "KRAKEN_DEPOSIT_LIMIT_UNVERIFIED",
                    "Kraken did not report an account-specific USD maximum for this tokenized deposit route.",
                ));
            }
            let maximum = &matching[0]["maximum_amount"];
            if maximum["asset"]["class"] != "currency"
                || !matches!(maximum["asset"]["name"].as_str(), Some("USD" | "ZUSD"))
            {
                return Err(error(
                    "KRAKEN_DEPOSIT_LIMIT_DENOMINATION_UNVERIFIED",
                    "Tokenized account limits must be explicitly returned in USD.",
                ));
            }
            let value = self
                .kraken
                .value_funding_composition(
                    &[ResourceAmount {
                        resource: account.resource.clone(),
                        amount: Amount {
                            asset: venue_asset.clone(),
                            atoms: gross.clone(),
                        },
                    }],
                    &[],
                )
                .await?;
            let usd = native_quantity(text(maximum, "amount")?, 36)?.mul_div_floor(
                &Quantity::from_u64(1_000_000),
                &Quantity::new(format!("1{}", "0".repeat(36)))?,
            )?;
            if value.movement_usd_micros > usd {
                return Err(error(
                    "KRAKEN_DEPOSIT_LIMIT",
                    "Exact base-tokenized USD deposit value exceeds this account's current limit.",
                ));
            }
        } else if has_account_maximum
            && (matching[0]["maximum_amount"]["asset"]["name"] != coin
                || matching[0]["maximum_amount"]["asset"]["class"] != asset_class
                || native_quantity(text(&matching[0]["maximum_amount"], "amount")?, 36)? < gross)
        {
            return Err(error(
                "KRAKEN_DEPOSIT_LIMIT",
                "Gross deposit exceeds or differs from this method/account's current limit.",
            ));
        }
        let inner_input = if solana_source {
            let mut inner = json!({"wallet":input.wallet,"network":"solana","cluster":fin_solana::CLUSTER,"requestId":input.request_id,"max_fee_sol":input.max_fee_sol,"amount":input.amount,"recipient":recipient});
            if source_asset.reference != "native" {
                inner["mint"] = json!(source_asset.reference);
                let wallet = self.solana.get(&input.wallet).ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_WALLET_MISSING",
                        "Original Solana source wallet is no longer configured.",
                    )
                })?;
                let source_account = fin_solana::associated_token_address(
                    wallet.address(),
                    &source_asset.reference,
                    fin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                inner["source_token_account"] = json!(source_account);
            }
            if let Some(cap) = &input.max_account_funding_sol {
                inner["max_account_funding_sol"] = json!(cap);
            }
            inner
        } else {
            json!({"wallet":input.wallet,"network":"evm","chain_id":input.chain_id,"request_id":input.request_id,"max_fee_eth":input.max_fee_eth,"token":source_asset.reference,"amount":input.amount,"recipient":recipient})
        };
        let inner = if let Some(original) = original {
            source_adapter.revalidate(original).await?
        } else {
            source_adapter
                .prepare("wallet.transfer", &inner_input)
                .await?
        };
        let source = inner.intent.debits.first().ok_or_else(|| {
            error(
                "KRAKEN_DEPOSIT_INNER_INVALID",
                "Wallet preview omitted its source debit.",
            )
        })?;
        if source.resource != wallet.resource
            || source.amount.asset != source_asset
            || source.amount.atoms != source_atoms
        {
            return Err(error(
                "KRAKEN_DEPOSIT_SOURCE_MISMATCH",
                "Wallet preview changed the source resource or exact asset quantity.",
            ));
        }
        if separate_fee && self.free(&input.account, &fee_asset).await? < fee {
            return Err(error(
                "INSUFFICIENT_BALANCE",
                "Available Kraken fee-asset cash cannot cover this deposit's separate fee.",
            ));
        }
        let route = json!({"method":method,"address":address,"limits":matching[0],"fee":q});
        let free_before = self.free(&input.account, &venue_asset).await?;
        let binding = DepositBinding {
            usd_ceiling: None,
            account: input.account.clone(),
            discovery_api: discovery_api(),
            credit_api: credit_api(),
            account_resource: account.resource.clone(),
            source_kind: if solana_source {
                "solana".into()
            } else {
                "evm".into()
            },
            cluster: solana_source.then(|| fin_solana::CLUSTER.into()),
            mint: (solana_source && source_asset.reference != "native")
                .then(|| source_asset.reference.clone()),
            physical_identity: account.details["physical_identity"]
                .as_str()
                .map(str::to_owned),
            method_id: input.method_id.clone(),
            method_name: text(method, "method_name")?.into(),
            network_id: text(&method["network"], "network_id")?.into(),
            network_name: network_name.into(),
            chain_id: input.chain_id,
            address: recipient.into(),
            address_id: address.get("id").and_then(Value::as_str).map(str::to_owned),
            venue_asset: venue_asset.clone(),
            source_asset: source_asset.clone(),
            gross: gross.clone(),
            net: net.clone(),
            fee: fee.clone(),
            fee_asset: Some(fee_asset.clone()),
            free_before,
            route,
            inner_input,
            inner: inner.clone(),
        };
        let mut locks = vec![source.resource.clone(), account.resource.clone()];
        locks.sort();
        locks.dedup();
        let mut maximum_costs = inner.intent.maximum_costs.clone();
        {
            maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: account.resource.clone(),
                },
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: fee.clone(),
                },
                treatment: if separate_fee {
                    CostTreatment::AdditionalDebit
                } else {
                    CostTreatment::DeductedFromCredit
                },
            });
        }
        let mut expected = vec![EffectConstraint::Transfer {
            from: source.resource.clone(),
            to: account.resource.clone(),
            debit_asset: source_asset.clone(),
            credit_asset: venue_asset.clone(),
            maximum_debit: source_atoms.clone(),
            minimum_credit: net.clone(),
        }];
        if solana_source {
            // Solana SPL transfers may need a new recipient ATA. The wallet
            // preview binds its exact creation/rent transfer separately from
            // the Kraken custody credit; carry that typed constraint through
            // approval so the rent is neither hidden nor mistaken for deposit.
            expected.extend(inner.intent.expected.iter().skip(1).cloned());
        }
        let mut debits = vec![ResourceAmount {
            resource: source.resource.clone(),
            amount: source.amount.clone(),
        }];
        if solana_source {
            for effect in inner.intent.expected.iter().skip(1) {
                if let EffectConstraint::Transfer {
                    from,
                    debit_asset,
                    maximum_debit,
                    ..
                } = effect
                    && !maximum_debit.is_zero()
                {
                    debits.push(ResourceAmount {
                        resource: from.clone(),
                        amount: Amount {
                            asset: debit_asset.clone(),
                            atoms: maximum_debit.clone(),
                        },
                    });
                }
            }
        }
        let intent = FinancialIntent {
            debits,
            maximum_costs,
            expected,
            locks,
        };
        let valuation = Some(
            self.kraken
                .value_funding_composition(&intent.debits, &intent.maximum_costs)
                .await?,
        );
        let mut warnings = vec!["Source receipt does not complete this deposit. Exactly correlated Kraken usable credit and ledger proof are required.".into()];
        let limit_detail = if has_account_maximum {
            "Existing claimed address, exact network/contract, minimum, fee and account maximum were independently checked."
        } else {
            warnings.push("Kraken did not return an account-specific deposit maximum for this route; only the exact approved principal and Fin fee/gas bounds limit this transfer.".into());
            "Existing claimed address, exact network/contract, minimum and fee were checked; Kraken reported no applicable account maximum."
        };
        let mut prepared = PreparedOperation {
            operation: "kraken.deposit".into(),
            input: raw_input.clone(),
            binding: serde_json::to_value(binding)
                .map_err(|_| FinError::validation("Deposit binding serialization failed."))?,
            summary: format!(
                "Deposit {} {} from {} to Kraken {} on {}; maximum chain fee {} {}",
                input.amount,
                coin,
                input.wallet,
                input.account,
                network_name,
                if solana_source {
                    input.max_fee_sol.as_deref().unwrap_or("0")
                } else {
                    &input.max_fee_eth
                },
                if solana_source { "SOL" } else { "ETH" }
            ),
            checks: vec![Check {
                name: "funding_route".into(),
                status: CheckStatus::Passed,
                detail: limit_detail.into(),
            }],
            warnings,
            observed_at: inner.observed_at,
            expires_at: inner.expires_at,
            valuation,
            intent,
            operation_version: 1,
            action_deadline: None,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}
#[async_trait]
impl Adapter for KrakenEvmDepositAdapter {
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        operation == "kraken.deposit"
            && Self::input(input).is_ok_and(|input| input.network.is_none() && input.chain_id != 0)
    }
    fn definitions(&self) -> Vec<Definition> {
        vec![deposit_definition()]
    }
    fn authorization_identities(&self) -> Vec<fin_domain::ConnectionMetadata> {
        let mut rows = self.kraken.authorization_identities();
        for a in self.ethereum.values() {
            rows.extend(a.authorization_identities());
        }
        for wallet in self.solana.values() {
            rows.extend(wallet.authorization_identities());
        }
        rows
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        })
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "Kraken deposit is a financial write; preview it first.",
        ))
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != "kraken.deposit" {
            return Err(FinError::validation("Unsupported deposit operation."));
        }
        self.prepare_deposit(input, None).await
    }
    async fn revalidate(&self, p: &PreparedOperation) -> Result<PreparedOperation> {
        if p.operation != "kraken.deposit"
            || p.operation_version != 1
            || DateTime::parse_from_rfc3339(&p.expires_at).map_or(true, |t| t <= Utc::now())
        {
            return Err(error(
                "PREVIEW_EXPIRED",
                "The original Kraken deposit preview expired or is invalid; nothing was submitted.",
            ));
        }
        let old: DepositBinding = decode_binding(&p.binding).map_err(|_| {
            error(
                "KRAKEN_DEPOSIT_BINDING_INVALID",
                "Original deposit binding is malformed.",
            )
        })?;
        let mut fresh = self.prepare_deposit(&p.input, Some(&old.inner)).await?;
        let current: DepositBinding = decode_binding(&fresh.binding).map_err(|_| {
            error(
                "KRAKEN_DEPOSIT_BINDING_INVALID",
                "Fresh deposit binding is malformed.",
            )
        })?;
        if old.route != current.route
            || old.inner_input != current.inner_input
            || old.inner.binding != current.inner.binding
            || old.free_before != current.free_before
            || old.account_resource != current.account_resource
            || old.source_asset != current.source_asset
            || old.physical_identity != current.physical_identity
            || old.fee_asset != current.fee_asset
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "Kraken route, account, fee, destination, source balance or wallet envelope changed.",
            ));
        }
        fin_domain::value_bounds::carry(p, &mut fresh)?;
        fresh.binding = p.binding.clone();
        Ok(fresh)
    }
    async fn submit(
        &self,
        p: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let fresh = self.revalidate(p).await?;
        let validated = RevalidatedOperation::after_revalidation(
            fresh,
            Arc::new(fin_domain::clock::SystemClock),
        )?;
        self.submit_revalidated(&validated, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let p = validated.prepared();
        let b: DepositBinding = decode_binding(&p.binding).map_err(|_| {
            error(
                "KRAKEN_DEPOSIT_BINDING_INVALID",
                "Deposit binding is malformed.",
            )
        })?;
        if p.operation != "kraken.deposit"
            || p.operation_version != 1
            || b.inner.operation != "wallet.transfer"
            || b.inner.operation_version != 1
            || b.inner.input != b.inner_input
        {
            return Err(error(
                "KRAKEN_DEPOSIT_BINDING_INVALID",
                "Deposit does not bind an exact wallet transfer.",
            ));
        }
        // Revalidation checked the nested envelope and fresh pricing. Preserve
        // the historical nested preview in the immutable approval binding and
        // keep the freshly validated outer candidate at signing and dispatch.
        journal.assert_candidate(p)?;
        if b.source_kind == "solana" {
            let wallet = b
                .inner_input
                .get("wallet")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_BINDING_INVALID",
                        "Solana wallet identity is missing from the original binding.",
                    )
                })?;
            return self
                .solana
                .get(wallet)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_DEPOSIT_WALLET_MISSING",
                        "Original Solana source wallet is no longer configured.",
                    )
                })?
                .submit_nested_revalidated(validated, &b.inner, journal)
                .await;
        }
        let transaction: fin_evm::FrozenTransaction =
            serde_json::from_value(b.inner.binding["transaction"].clone()).map_err(|_| {
                error(
                    "KRAKEN_DEPOSIT_BINDING_INVALID",
                    "Deposit transaction is malformed.",
                )
            })?;
        let resource = b.inner.binding["resource"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_BINDING_INVALID",
                    "Deposit source resource is malformed.",
                )
            })?;
        self.ethereum
            .get(&b.chain_id)
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_CHAIN_MISSING",
                    "Original chain configuration is missing.",
                )
            })?
            .runtime
            .submit_nested_revalidated(
                &transaction,
                json!({"operation":"wallet.transfer","resource":resource}),
                journal,
                validated,
                "/inner/binding",
            )
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(preview) = &record.preview else {
            return Ok(unknown("Original deposit preview is missing.", json!({})));
        };
        let b: DepositBinding = decode_binding(&preview.binding).map_err(|_| {
            error(
                "KRAKEN_DEPOSIT_BINDING_INVALID",
                "Original deposit binding is malformed.",
            )
        })?;
        if b.discovery_api != "funding-v1-v2" || b.credit_api != "legacy-v0" {
            return Ok(unknown(
                "Original deposit API version is unsupported; recovery cannot switch its identity source.",
                json!({"discoveryApi":b.discovery_api,"creditApi":b.credit_api}),
            ));
        }
        let Some(submission) = &record.submission else {
            return Ok(unknown(
                "Original source transaction identity is missing.",
                json!({}),
            ));
        };
        // Solana's public submission reference identifies the frozen message,
        // while venue histories use the transaction's case-sensitive
        // base58 signature. EVM submission references remain transaction hashes.
        let venue_txid = if b.source_kind == "solana" {
            submission.data["wallet_signature"]
                .as_str()
                .filter(|signature| !signature.is_empty())
                .unwrap_or(&submission.reference)
        } else {
            &submission.reference
        };
        let source_adapter: &dyn Adapter = if b.source_kind == "solana" {
            let Some(wallet) = b
                .inner_input
                .get("wallet")
                .and_then(Value::as_str)
                .and_then(|name| self.solana.get(name))
            else {
                return Ok(unknown(
                    "Original Solana source wallet is no longer configured.",
                    json!({}),
                ));
            };
            wallet.as_ref()
        } else if let Some(ethereum) = self.ethereum.get(&b.chain_id) {
            ethereum.as_ref()
        } else {
            return Ok(unknown(
                "Original source chain is no longer configured.",
                json!({}),
            ));
        };
        if !self.kraken.connections().iter().any(|a| {
            a.name == b.account
                && a.resource == b.account_resource
                && b.physical_identity
                    .as_deref()
                    .is_some_and(|id| a.details["physical_identity"] == id)
        }) {
            return Ok(unknown(
                "Original Kraken account identity changed.",
                json!({}),
            ));
        }
        let inner_record = OperationRecord {
            operation: "wallet.transfer".into(),
            input: b.inner_input.clone(),
            preview: Some(b.inner.clone()),
            ..record.clone()
        };
        let chain = match source_adapter.reconcile(&inner_record).await {
            Ok(chain) => chain,
            Err(source_error) => {
                return Ok(unknown(
                    "Original source transaction reconciliation returned an error; keep the original identity and do not replay.",
                    json!({
                        "stage":source_error.stage,
                        "code":source_error.code,
                        "message":source_error.message
                    }),
                ));
            }
        };
        if chain.status != Status::Completed {
            return Ok(chain);
        }
        let Some(mut proof) = chain.verified else {
            return Ok(unknown(
                "Canonical source receipt omitted typed effects.",
                chain.evidence,
            ));
        };
        if proof.submission != *submission {
            return Ok(unknown(
                "Source proof identity differs from original transaction.",
                chain.evidence,
            ));
        }
        // Current Funding deposit history omits onchain tx IDs. The persisted
        // binding explicitly selects legacy status correlation for its original
        // credit proof; current API discovery does not alter this recovery rule.
        let status = self
            .read(E::KrakenPrivatePostDepositStatus, &b.account, {
                let mut query = json!({"asset":b.venue_asset.reference,"limit":500});
                if let Some(unit) = b
                    .venue_asset
                    .network
                    .strip_prefix("kraken:tokenized_asset:")
                {
                    query["aclass"] = json!("tokenized_asset");
                    query["rebase_multiplier"] = json!(unit);
                }
                query
            })
            .await?;
        let rows = raw(&status)?
            .as_array()
            .or_else(|| raw(&status).ok().and_then(|r| r["deposit"].as_array()))
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_HISTORY_INVALID",
                    "Deposit history is not a bounded list.",
                )
            })?;
        let found: Vec<_> = rows
            .iter()
            .filter(|r| {
                r["txid"].as_str().is_some_and(|tx| {
                    if b.source_kind == "solana" {
                        tx == venue_txid
                    } else {
                        tx.eq_ignore_ascii_case(&submission.reference)
                    }
                })
            })
            .collect();
        if found.len() != 1 {
            return Ok(unknown(
                "Exactly one venue deposit must correlate to the original source transaction.",
                json!({"matchingRows":found.len(),"chain":chain.evidence}),
            ));
        }
        let row = found[0];
        let route_matches = row["asset"] == b.venue_asset.reference
            && row["method"]
                .as_str()
                .is_some_and(|reported| deposit_status_method_matches(&b.method_name, reported))
            && row["info"].as_str().is_some_and(|a| {
                if b.source_kind == "solana" {
                    a == b.address
                } else {
                    a.eq_ignore_ascii_case(&b.address)
                }
            });
        // Kraken's documented Settled deposit status still awaits confirmations.
        // Keep the original submission pending without manufacturing usable credit.
        if row["status"] == "Settled"
            && route_matches
            && row
                .get("status-prop")
                .is_none_or(|property| property.is_null() || property.as_str() == Some(""))
            && row["refid"].as_str().is_some_and(|s| !s.is_empty())
            && row["amount"].as_str().is_some_and(|amount| {
                native_quantity(amount, 36).is_ok_and(|q| q == b.gross || q == b.net)
            })
        {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({
                    "reason":"Kraken received the matching deposit and is waiting for additional blockchain confirmations before usable credit.",
                    "awaiting":"venue_usable_credit",
                    "detail":{"venue":row,"chain":chain.evidence},
                    "resubmissionAllowed":false
                }),
                verified: None,
            });
        }
        // Kraken's EarlyConfirmed status means the deposit is already credited
        // for trading. The exact txid/method/address/refid, correlated ledger,
        // and free-balance checks below remain mandatory before settlement.
        if !(row["status"] == "Success" || row["status"] == "EarlyConfirmed")
            || row["status-prop"] == "onhold"
            || row["status-prop"] == "return"
            || !route_matches
        {
            return Ok(unknown(
                "Venue deposit is not one exact usable route credit.",
                json!({
                    "venue":row,
                    "chain":chain.evidence,
                    "checks":{
                        "status_usable":row["status"]=="Success"||row["status"]=="EarlyConfirmed",
                        "not_on_hold":row["status-prop"]!="onhold",
                        "not_returned":row["status-prop"]!="return",
                        "asset_matches":row["asset"]==b.venue_asset.reference,
                        "method_matches":row["method"].as_str().is_some_and(|reported|deposit_status_method_matches(&b.method_name,reported)),
                        "address_matches":row["info"].as_str().is_some_and(|a| if b.source_kind == "solana" { a == b.address } else { a.eq_ignore_ascii_case(&b.address) })
                    }
                }),
            ));
        }
        let fee_asset = b.fee_asset.as_ref().unwrap_or(&b.venue_asset);
        let separate_fee = fee_asset != &b.venue_asset;
        let history_amount = native_quantity(text(row, "amount")?, 36)?;
        if text(row, "refid")?.is_empty() {
            return Ok(unknown(
                "A deposit reference is required for independent ledger correlation.",
                json!(row),
            ));
        }
        let ledger = self
            .read(E::KrakenPrivatePostLedgers, &b.account, {
                let mut query = json!({"asset":b.venue_asset.reference,"without_count":true});
                if let Some(unit) = b
                    .venue_asset
                    .network
                    .strip_prefix("kraken:tokenized_asset:")
                {
                    query["aclass"] = json!("tokenized_asset");
                    query["rebase_multiplier"] = json!(unit);
                }
                query
            })
            .await?;
        let entries = raw(&ledger)?
            .get("ledger")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_LEDGER_INVALID",
                    "Correlated deposit ledger is missing.",
                )
            })?;
        let entries: Vec<_> = entries
            .iter()
            .filter(|(_, v)| {
                v["refid"] == row["refid"]
                    && v["asset"] == b.venue_asset.reference
                    && v["type"] == "deposit"
            })
            .collect();
        let ledger_amount = if entries.len() == 1 {
            native_quantity(text(entries[0].1, "amount")?, 36)?
        } else {
            fin_domain::Quantity::zero()
        };
        if entries.len() != 1 || (ledger_amount != b.gross && ledger_amount != b.net) {
            return Ok(unknown(
                "Deposit credit lacks a unique exact ledger identity.",
                json!({"matchingLedgerRows":entries.len()}),
            ));
        }
        let venue_ledger_evidence = exact_ledger_evidence(entries[0].0, entries[0].1);
        let credit_fee = native_quantity(text(entries[0].1, "fee")?, 36)?;
        if (separate_fee && !credit_fee.is_zero()) || credit_fee > b.gross {
            return Ok(unknown(
                "Deposit ledger does not prove the original fee denomination and credit treatment.",
                json!({"entry":entries[0].1}),
            ));
        }
        let actual_net = b.gross.checked_sub(&credit_fee)?;
        if (history_amount != actual_net && history_amount != b.gross)
            || ledger_amount != history_amount
            || (row.get("fee").is_some() && native_quantity(text(row, "fee")?, 36)? != credit_fee)
        {
            return Ok(unknown(
                "Deposit history and independently correlated gross/net fee ledger disagree.",
                json!({"deposit":row,"entry":entries[0].1}),
            ));
        }
        let mut actual_fee = credit_fee;
        let mut fee_proof = Value::Null;
        if separate_fee {
            let mut query = json!({"asset":fee_asset.reference,"without_count":true});
            if let Some(unit) = fee_asset.network.strip_prefix("kraken:tokenized_asset:") {
                query["aclass"] = json!("tokenized_asset");
                query["rebase_multiplier"] = json!(unit);
            }
            let response = self
                .read(E::KrakenPrivatePostLedgers, &b.account, query)
                .await?;
            let ledger = raw(&response)?["ledger"].as_object().ok_or_else(|| {
                error(
                    "KRAKEN_DEPOSIT_LEDGER_INVALID",
                    "Deposit fee asset ledger evidence is absent.",
                )
            })?;
            let fees = ledger
                .iter()
                .filter(|(_, v)| v["refid"] == row["refid"] && v["asset"] == fee_asset.reference)
                .collect::<Vec<_>>();
            if fees.len() != 1 {
                return Ok(unknown(
                    "Deposit's separate fee asset needs one independently correlated exact debit.",
                    json!({"matchingFeeRows":fees.len()}),
                ));
            }
            let amount = text(fees[0].1, "amount")?;
            if !amount.starts_with('-') || amount.starts_with("-+") {
                return Ok(unknown(
                    "Correlated deposit fee debit differs from the original exact cap and denomination.",
                    json!(fees[0].1),
                ));
            }
            actual_fee = native_quantity(amount.strip_prefix('-').unwrap_or(amount), 36)?
                .checked_add(&native_quantity(text(fees[0].1, "fee")?, 36)?)?;
            fee_proof = exact_ledger_evidence(fees[0].0, fees[0].1);
        }
        let free_after = self.free(&b.account, &b.venue_asset).await?;
        if free_after < b.free_before.checked_add(&actual_net)? {
            return Ok(unknown(
                "The correlated deposit is not yet corroborated by usable account cash.",
                json!({"before":b.free_before,"after":free_after}),
            ));
        }
        let source =
            b.inner.intent.debits.first().ok_or_else(|| {
                error("KRAKEN_DEPOSIT_INNER_INVALID", "Source intent is missing.")
            })?;
        let expected_recipient = if b.source_kind == "solana" {
            ResourceId(format!("solana:{}:{}", fin_solana::CLUSTER, b.address))
        } else {
            ResourceId(format!(
                "ethereum:{}:{}",
                b.chain_id,
                b.address.to_ascii_lowercase()
            ))
        };
        let mut effects = vec![];
        let mut matched = false;
        for effect in proof.effects {
            match effect {
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit,
                    credit,
                } if !matched
                    && from == source.resource
                    && to == expected_recipient
                    && debit == source.amount
                    && credit.asset == b.source_asset
                    && credit.atoms == debit.atoms =>
                {
                    matched = true;
                    effects.push(FinancialEffect::Transfer {
                        from,
                        to: b.account_resource.clone(),
                        debit,
                        credit: Amount {
                            asset: b.venue_asset.clone(),
                            atoms: b.gross.clone(),
                        },
                    });
                }
                FinancialEffect::Cost { .. } => effects.push(effect),
                FinancialEffect::Transfer { ref to, .. }
                    if b.source_kind == "solana"
                        && to.0.starts_with("solana:mainnet-beta:account:") =>
                {
                    // Preserve independently verified ATA rent funding as its
                    // own typed movement; it is not exchange asset custody.
                    effects.push(effect);
                }
                _ => {
                    return Ok(unknown(
                        "Unexpected source effects cannot be translated into Kraken custody.",
                        chain.evidence,
                    ));
                }
            }
        }
        if !matched {
            return Ok(unknown(
                "Canonical source effects omitted the exact deposit transfer.",
                chain.evidence,
            ));
        }
        if !actual_fee.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: b.account_resource.clone(),
                },
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: actual_fee.clone(),
                },
                category: "kraken_deposit_fee".into(),
                treatment: if separate_fee {
                    CostTreatment::AdditionalDebit
                } else {
                    CostTreatment::DeductedFromCredit
                },
            });
        }
        if actual_fee > b.fee {
            // A receipt with excessive native fees does not authorize custody
            // credit. Preserve only independently proved costs and identities.
            let costs: Vec<_> = effects
                .iter()
                .filter(|effect| {
                    matches!(effect,
                        FinancialEffect::Cost { amount, .. } if !amount.atoms.is_zero()
                    )
                })
                .cloned()
                .collect();
            let mut events = Vec::new();
            for effect in &costs {
                if let FinancialEffect::Cost {
                    category,
                    location,
                    amount,
                    treatment,
                } = effect
                    && category != "kraken_deposit_fee"
                {
                    events.push(json!({"eventId":format!("fin:deposit-source-cost:{}:{}",submission.reference,binding_hash(&json!({"category":category,"location":location,"asset":amount.asset,"treatment":treatment}))?),"submission":submission,"cost":effect}));
                }
            }
            events.push(json!({"eventId":format!("kraken:deposit-ledger:{}:{}",b.account_resource.0,entries[0].0),"entry":venue_ledger_evidence}));
            if separate_fee {
                events.push(json!({"eventId":format!("kraken:deposit-fee-ledger:{}:{}",b.account_resource.0,text(&fee_proof,"identity")?),"entry":fee_proof}));
            }
            let evidence = json!({"source":chain.evidence,"venueDeposit":exact_deposit_evidence(row),"venueLedger":[venue_ledger_evidence],"feeLedger":fee_proof,"freeBefore":b.free_before,"freeAfter":free_after,"routeDigest":deposit_route_digest(&b)? ,"proofEvents":events});
            let fee_location = AccountingLocation::Cash {
                resource: b.account_resource.clone(),
            };
            let treatment = if separate_fee {
                CostTreatment::AdditionalDebit
            } else {
                CostTreatment::DeductedFromCredit
            };
            if !preview.intent.maximum_costs.iter().any(|cap| {
                cap.location == fee_location
                    && cap.amount.asset == *fee_asset
                    && cap.treatment == treatment
            }) || !preview
                .valuation
                .as_ref()
                .is_some_and(|value| value.prices.iter().any(|price| price.asset == *fee_asset))
            {
                return Ok(unknown(
                    "The original deposit record lacks an approved fee identity or its original price. Keep its source identity and reservations; legacy excess-cost accounting is incomplete.",
                    evidence,
                ));
            }
            let consumed = fin_domain::unresolved_consumption::UnresolvedCostConsumption::new(
                submission.clone(),
                costs,
                evidence.clone(),
                now(),
            )?;
            let mut result = unknown(
                "Correlated deposit costs exceed the approved fee scope; preserve the original source identity and reservations.",
                evidence,
            );
            fin_domain::unresolved_consumption::attach(&mut result, &consumed)?;
            return Ok(result);
        }
        if actual_fee != b.fee || actual_net != b.net {
            return Ok(unknown(
                "Deposit fee or net credit differs from the original exact scope.",
                json!({"deposit":row,"feeLedger":fee_proof}),
            ));
        }
        proof.effects = effects;
        proof.evidence = json!({"method":"canonical-evm-and-kraken-credit-v1","source":chain.evidence,"venueDeposit":exact_deposit_evidence(row),"venueLedger":[venue_ledger_evidence],"feeLedger":fee_proof,"freeBefore":b.free_before,"freeAfter":free_after,"routeDigest":deposit_route_digest(&b)? ,"creditApi":"legacy-v0","discoveryApi":"funding-v1-v2"});
        proof.observed_at = now();
        proof.proof_digest = verified_effect_digest(&proof)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: proof.evidence.clone(),
            verified: Some(proof),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fin_exchange::kraken::DispatchGuard;
    use fin_kraken::{KrakenAccount, KrakenProduct, KrakenResponse, KrakenTransport};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    mod cost_tests {
        include!("kraken_deposit_cost_tests.rs");
    }
    pub(crate) mod submission_tests {
        include!("kraken_deposit_submission_tests.rs");
    }
    struct Venue {
        memo: AtomicBool,
        wrong_network: AtomicBool,
        fee: AtomicBool,
        credit: AtomicBool,
        early_confirmed: AtomicBool,
        deposit_status_override: Mutex<Value>,
        tokenized: AtomicBool,
        separate_fee: AtomicBool,
        fee_proven: AtomicBool,
        readable_credit: AtomicBool,
        fee_overrun: AtomicBool,
        solana_source: AtomicBool,
        solana_usdc: AtomicBool,
        solana_signature: Arc<Mutex<Option<String>>>,
        solana_rpc: Arc<Mutex<Option<Arc<crate::test_support::solana::FundingRpc>>>>,
        multiple_claimed_addresses: AtomicBool,
        no_account_deposit_maximum: AtomicBool,
        requests: Mutex<Vec<(E, Value)>>,
        rpc: Arc<Rpc>,
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
            assert!(
                !e.mutating(),
                "Deposit preview must not create an address or withdraw"
            );
            assert!(guard.is_none());
            self.requests.lock().unwrap().push((e, parameters.clone()));
            let separate = self.separate_fee.load(Ordering::SeqCst);
            let fees = self.fee.load(Ordering::SeqCst) && !separate;
            let credited = self.credit.load(Ordering::SeqCst)
                || (!self.solana_source.load(Ordering::SeqCst)
                    && self.rpc.submitted_hash.lock().unwrap().is_some());
            let mut response = match e {
                E::KrakenPrivatePostGetApiKeyInfo => {
                    json!({"result":{"permissions":if self.readable_credit.load(Ordering::SeqCst){vec!["query-funds","query-ledger"]}else{vec!["query-funds"]}}})
                }
                E::KrakenFundingGetFundingV1MethodsDirection => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","method_name":"USDC (Solana)","asset":{"class":"currency","name":"USDC"},"network":{"network_id":"sol-mainnet","network_name":"Solana","contract_address":crate::test_support::solana::USDC_MINT},"minimum_amount":"0.01","maximum_amount":"100000","fees":{"base":{"asset":{"class":"currency","name":"USDC"},"amount":"0"},"included":false},"deposit":{"shares_addresses_with_method_id":"00000000-0000-0000-0000-000000000003"}}]})
                        } else {
                            json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","method_name":"Solana","asset":{"class":"currency","name":"SOL"},"network":{"network_id":"sol-mainnet","network_name":if self.wrong_network.load(Ordering::SeqCst){"Solana Devnet"}else{"Solana"}},"minimum_amount":"0.001","maximum_amount":"100","fees":{"base":{"asset":{"class":"currency","name":"SOL"},"amount":"0.001"},"included":true},"deposit":{"shares_addresses_with_method_id":"00000000-0000-0000-0000-000000000003"}}]})
                        }
                    } else {
                        json!({"methods":[{"method_id":"00000000-0000-0000-0000-000000000001","method_name":"Ether (Hex)","asset":{"class":"currency","name":"ETH"},"network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":if self.wrong_network.load(Ordering::SeqCst){"Bitcoin"}else{"Ethereum"}},"minimum_amount":"0.001","maximum_amount":"1","fees":{"base":{"asset":{"class":"currency","name":"ETH"},"amount":if fees{"0.0001"}else{"0"}},"included":true},"deposit":{"shares_addresses_with_method_id":"00000000-0000-0000-0000-000000000003"}}]})
                    }
                }
                E::KrakenPublicGetAssets => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"result":{"USDC":{"altname":"USDC","decimals":6}}})
                        } else {
                            json!({"result":{"SOL":{"altname":"SOL","decimals":9}}})
                        }
                    } else {
                        json!({"result":{"XETH":{"altname":"ETH","decimals":18}}})
                    }
                }
                E::KrakenFundingGetFundingV2DepositAddresses => {
                    let mut addresses = vec![
                        json!({"method_id":"00000000-0000-0000-0000-000000000003","address_details":{"crypto":{"address":if self.solana_source.load(Ordering::SeqCst){"11111111111111111111111111111111"}else{"0x1111111111111111111111111111111111111111"},"memo":if self.memo.load(Ordering::SeqCst){"required-memo"}else{""}}}}),
                    ];
                    if self.multiple_claimed_addresses.load(Ordering::SeqCst) {
                        addresses.push(json!({"method_id":"00000000-0000-0000-0000-000000000003","address_details":{"crypto":{"address":"0x2222222222222222222222222222222222222222"}}}));
                    }
                    json!({"addresses":addresses})
                }
                E::KrakenFundingGetFundingV1FeesMethodId => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"fee":{"asset":{"class":"currency","name":"USDC"},"amount":"0"},"gross_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1"},"net_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1"}})
                        } else {
                            json!({"fee":{"asset":{"class":"currency","name":"SOL"},"amount":"0.001"},"gross_amount":{"asset":{"class":"currency","name":"SOL"},"amount":"0.1"},"net_amount":{"asset":{"class":"currency","name":"SOL"},"amount":"0.099"}})
                        }
                    } else {
                        json!({"fee":{"asset":{"class":"currency","name":"ETH"},"amount":if fees{"0.0001"}else{"0"}},"gross_amount":{"asset":{"class":"currency","name":"ETH"},"amount":"0.01"},"net_amount":{"asset":{"class":"currency","name":"ETH"},"amount":if fees{"0.0099"}else{"0.01"}}})
                    }
                }
                E::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset => {
                    if self.no_account_deposit_maximum.load(Ordering::SeqCst) {
                        json!({"deposit_limits":[{"method_id":"00000000-0000-0000-0000-000000000001","limits":[]}]})
                    } else if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"deposit_limits":[{"method_id":"00000000-0000-0000-0000-000000000001","maximum_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"100000"}}]})
                        } else {
                            json!({"deposit_limits":[{"method_id":"00000000-0000-0000-0000-000000000001","maximum_amount":{"asset":{"class":"currency","name":"SOL"},"amount":"100"}}]})
                        }
                    } else {
                        json!({"deposit_limits":[{"method_id":"00000000-0000-0000-0000-000000000001","maximum_amount":{"asset":{"class":"currency","name":"ETH"},"amount":"1"}}]})
                    }
                }
                E::KrakenPrivatePostBalanceEx => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"result":{"USDC":{"balance":if credited{"11"}else{"10"},"hold_trade":"0"}}})
                        } else {
                            json!({"result":{"SOL":{"balance":if credited{"10.099"}else{"10"},"hold_trade":"0"}}})
                        }
                    } else {
                        json!({"result":{"XETH":{"balance":if credited{if fees{"0.5099"}else{"0.51"}}else{"0.5"},"hold_trade":"0"}}})
                    }
                }
                E::KrakenPrivatePostDepositStatus => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        let signature = self
                            .solana_rpc
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|rpc| rpc.last_signature())
                            .or_else(|| self.solana_signature.lock().unwrap().clone())
                            .unwrap_or_default();
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"result":[{"asset":"USDC","method":"USDC (Solana)","refid":"fixture-deposit-ref","txid":signature,"info":"11111111111111111111111111111111","amount":"1","fee":"0","status":if !credited{"Settled"}else{"Success"}}]})
                        } else {
                            json!({"result":[{"asset":"SOL","method":"Solana","refid":"fixture-deposit-ref","txid":signature,"info":"11111111111111111111111111111111","amount":"0.099","fee":"0.001","status":if !credited{"Settled"}else{"Success"}}]})
                        }
                    } else {
                        json!({"result":[{"asset":"XETH","method":"Ether (Hex)","refid":"fixture-deposit-ref","txid":self.rpc.hash(),"info":"0x1111111111111111111111111111111111111111","amount":"0.01","fee":if fees{"0.0001"}else{"0"},"status":if !credited{"Settled"}else if self.early_confirmed.load(Ordering::SeqCst){"EarlyConfirmed"}else{"Success"}}]})
                    }
                }
                E::KrakenPrivatePostLedgers => {
                    if self.solana_source.load(Ordering::SeqCst) {
                        if self.solana_usdc.load(Ordering::SeqCst) {
                            json!({"result":{"ledger":{"deposit-ledger":{"type":"deposit","refid":"fixture-deposit-ref","asset":"USDC","amount":"1","fee":"0","balance":"11"}}}})
                        } else {
                            json!({"result":{"ledger":{"deposit-ledger":{"type":"deposit","refid":"fixture-deposit-ref","asset":"SOL","amount":"0.099","fee":"0.001","balance":"10.099"}}}})
                        }
                    } else {
                        json!({"result":{"ledger":{"deposit-ledger":{"type":"deposit","refid":"fixture-deposit-ref","asset":"XETH","amount":"0.01","fee":if fees{"0.0001"}else{"0"}}}}})
                    }
                }
                _ => panic!("Unexpected synthetic deposit endpoint {e:?}"),
            };
            if e == E::KrakenPrivatePostDepositStatus
                && let Some(fields) = self.deposit_status_override.lock().unwrap().as_object()
            {
                for (key, value) in fields {
                    response["result"][0][key] = value.clone();
                }
            }
            if self.tokenized.load(Ordering::SeqCst) {
                fn replace(v: &mut Value) {
                    match v {
                        Value::String(s) => {
                            *s = match s.as_str() {
                                "currency" => "tokenized_asset",
                                "ETH" => "SPYX",
                                "XETH" => "SPYX.T",
                                "Ether (Hex)" => "SPYX Ethereum",
                                other => other,
                            }
                            .into()
                        }
                        Value::Object(o) => {
                            for value in o.values_mut() {
                                replace(value);
                            }
                        }
                        Value::Array(a) => {
                            for value in a {
                                replace(value);
                            }
                        }
                        _ => {}
                    }
                }
                replace(&mut response);
                if e == E::KrakenPublicGetAssets {
                    response = json!({"result":{"SPYX.T":{"altname":"SPYX","aclass":"tokenized_asset","decimals":8}}});
                }
                if e == E::KrakenFundingGetFundingV1MethodsDirection {
                    response["methods"][0]["network"]["contract_address"] =
                        json!("0x3333333333333333333333333333333333333333");
                }
                if e == E::KrakenPrivatePostBalanceEx {
                    let row = response["result"]["XETH"].take();
                    response["result"].as_object_mut().unwrap().remove("XETH");
                    response["result"]["SPYX.T"] = row;
                }
                if e == E::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset {
                    response["deposit_limits"][0]["maximum_amount"] =
                        json!({"asset":{"class":"currency","name":"USD"},"amount":"1000"});
                }
            }
            if separate {
                if e == E::KrakenPublicGetAssets && parameters["asset"] == "USDC" {
                    response = json!({"result":{"USDC":{"aclass":"currency","decimals":6}}});
                }
                if e == E::KrakenFundingGetFundingV1FeesMethodId {
                    response["fee"] =
                        json!({"asset":{"class":"currency","name":"USDC"},"amount":"0.0001"});
                }
                if e == E::KrakenPrivatePostBalanceEx {
                    response["result"]["USDC"] = json!({"balance":"1","hold_trade":"0"});
                }
                if e == E::KrakenPrivatePostLedgers && parameters["asset"] == "USDC" {
                    response = if self.fee_proven.load(Ordering::SeqCst) {
                        json!({"result":{"ledger":{"separate-fee-debit":{"refid":"fixture-deposit-ref","type":"withdrawal","asset":"USDC","amount":"-0.0001","fee":"0"}}}})
                    } else {
                        json!({"result":{"ledger":{}}})
                    };
                }
            }
            if self.fee_overrun.load(Ordering::SeqCst) {
                if e == E::KrakenPrivatePostLedgers {
                    if separate && parameters["asset"] == "USDC" {
                        if response["result"]["ledger"]["separate-fee-debit"].is_object() {
                            response["result"]["ledger"]["separate-fee-debit"]["amount"] =
                                json!("-0.0002");
                        }
                    } else if !separate {
                        response["result"]["ledger"]["deposit-ledger"]["fee"] = json!("0.0002");
                    }
                }
                if !separate && e == E::KrakenPrivatePostDepositStatus {
                    response["result"][0]["fee"] = json!("0.0002");
                }
                if !separate
                    && e == E::KrakenPrivatePostBalanceEx
                    && self.credit.load(Ordering::SeqCst)
                {
                    response["result"]["XETH"]["balance"] = json!("0.5098");
                }
            }
            Ok(KrakenResponse::Json(response))
        }
    }
    struct Rpc {
        transaction: Mutex<Option<fin_evm::FrozenTransaction>>,
        submitted_hash: Mutex<Option<String>>,
        calls: Mutex<Vec<String>>,
        nonce: Mutex<u64>,
        receipt: AtomicBool,
        tokenized: AtomicBool,
    }
    impl Rpc {
        fn hash(&self) -> String {
            self.submitted_hash
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| format!("0x{}", "ab".repeat(32)))
        }
    }
    #[async_trait]
    impl fin_evm::EvmRpc for Rpc {
        async fn request(&self, method: &str, p: Value) -> Result<Value> {
            self.calls.lock().unwrap().push(method.into());
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_getCode" => {
                    if self.tokenized.load(Ordering::SeqCst)
                        && p[0] == "0x3333333333333333333333333333333333333333"
                    {
                        json!("0x6000")
                    } else {
                        json!("0x")
                    }
                }
                "eth_getBalance" => json!("0xde0b6b3a7640000"),
                "eth_getTransactionCount" => json!(format!("{:#x}", *self.nonce.lock().unwrap())),
                "eth_estimateGas" => json!("0x5208"),
                "eth_maxPriorityFeePerGas" => json!("0x989680"),
                "eth_blockNumber" => json!("0x65"),
                "eth_getBlockByNumber" => {
                    json!({"number":if p[0]=="0x64"{"0x64"}else{"0x65"},"hash":format!("0x{}","cd".repeat(32)),"baseFeePerGas":"0x3b9aca00"})
                }
                "eth_call" => {
                    if self.tokenized.load(Ordering::SeqCst) {
                        let data = p[0]["data"]
                            .as_str()
                            .or_else(|| p[0]["input"].as_str())
                            .unwrap_or("");
                        if data.starts_with("0x313ce567") {
                            json!(format!("0x{:064x}", 18))
                        } else if data.starts_with("0x95d89b41") || data.starts_with("0x06fdde03") {
                            json!(format!(
                                "0x{}20{}0453505958{}",
                                "0".repeat(62),
                                "0".repeat(62),
                                "0".repeat(56)
                            ))
                        } else if data.starts_with("0x70a08231") {
                            json!(format!("0x{:064x}", 1_000_000_000_000_000_000_u128))
                        } else {
                            json!(format!("0x{:064x}", 1))
                        }
                    } else {
                        json!("0x")
                    }
                }
                "eth_getTransactionReceipt" => {
                    if self.receipt.load(Ordering::SeqCst) {
                        let mut receipt = json!({"transactionHash":self.hash(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x3b9aca00","logs":[]});
                        if self.tokenized.load(Ordering::SeqCst) {
                            receipt["logs"] = json!([{"address":"0x3333333333333333333333333333333333333333","topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",format!("0x{}{}","0".repeat(24),"22".repeat(20)),format!("0x{}{}","0".repeat(24),"11".repeat(20))],"data":format!("0x{:064x}",10_000_000_000_000_000_u128),"transactionHash":format!("0x{}","ab".repeat(32)),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","logIndex":"0x0","removed":false}]);
                        }
                        receipt
                    } else {
                        Value::Null
                    }
                }
                "eth_getTransactionByHash" => {
                    let tx = self.transaction.lock().unwrap();
                    let tx = tx.as_ref().unwrap();
                    json!({"hash":self.hash(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","from":tx.sender,"to":tx.to,"chainId":format!("{:#x}",tx.chain_id),"type":"0x2","nonce":format!("{:#x}",tx.nonce),"gas":format!("{:#x}",tx.gas),"value":format!("{:#x}",fin_evm::u256(&tx.value)?),"input":tx.calldata,"maxFeePerGas":format!("{:#x}",fin_evm::u256(&tx.max_fee_per_gas)?),"maxPriorityFeePerGas":format!("{:#x}",fin_evm::u256(&tx.max_priority_fee_per_gas)?),"accessList":[]})
                }
                "eth_sendRawTransaction" => {
                    let raw = hex::decode(p[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                    let hash = format!("{:#x}", fin_evm::alloy::primitives::keccak256(raw));
                    *self.submitted_hash.lock().unwrap() = Some(hash.clone());
                    self.receipt.store(true, Ordering::SeqCst);
                    json!(hash)
                }
                _ => panic!("Unexpected synthetic deposit RPC {method}"),
            })
        }
    }
    struct Prices;
    #[async_trait]
    impl fin_pricing::UsdPriceSource for Prices {
        async fn quote(&self, _: &AssetId) -> Result<fin_pricing::UsdQuote> {
            Ok(fin_pricing::UsdQuote {
                usd_per_unit: "2000".into(),
                updated_at: now(),
                observed_at: now(),
                source: "synthetic-only-fixed-eth-price".into(),
            })
        }
    }
    #[async_trait]
    impl fin_ethereum::WalletValuation for Prices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            Ok((
                units.mul_div_ceil(
                    &Quantity::from_u64(2_000_000_000),
                    &Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?,
                )?,
                "synthetic-only-fixed-eth-price".into(),
            ))
        }
    }
    fn adapter() -> (KrakenEvmDepositAdapter, Arc<Venue>) {
        let rpc = Arc::new(Rpc {
            transaction: Mutex::new(None),
            submitted_hash: Mutex::new(None),
            calls: Mutex::new(vec![]),
            nonce: Mutex::new(0),
            receipt: AtomicBool::new(false),
            tokenized: AtomicBool::new(false),
        });
        let venue = Arc::new(Venue {
            memo: AtomicBool::new(false),
            wrong_network: AtomicBool::new(false),
            fee: AtomicBool::new(false),
            credit: AtomicBool::new(false),
            early_confirmed: AtomicBool::new(false),
            deposit_status_override: Mutex::new(Value::Null),
            tokenized: AtomicBool::new(false),
            separate_fee: AtomicBool::new(false),
            fee_proven: AtomicBool::new(true),
            readable_credit: AtomicBool::new(true),
            fee_overrun: AtomicBool::new(false),
            solana_source: AtomicBool::new(false),
            solana_usdc: AtomicBool::new(false),
            solana_signature: Arc::new(Mutex::new(None)),
            solana_rpc: Arc::new(Mutex::new(None)),
            multiple_claimed_addresses: AtomicBool::new(false),
            no_account_deposit_maximum: AtomicBool::new(false),
            requests: Mutex::new(vec![]),
            rpc: rpc.clone(),
        });
        let kraken = Arc::new(
            KrakenProfile::new(
                KrakenProduct::Spot,
                vec![KrakenAccount {
                    name: "fixture-spot".into(),
                    resource: ResourceId("kraken:fixture-spot".into()),
                    credential_ref: "synthetic-credential-only".into(),
                    physical_identity: "synthetic-uid-only".into(),
                    wallet_account_id: None,
                    default_wallet_account_id: None,
                    identity_verified: true,
                    product: KrakenProduct::Spot,
                }],
                venue.clone(),
            )
            .unwrap()
            .with_prices(Arc::new(Prices)),
        );
        let ethereum = Arc::new(
            fin_ethereum::EthereumAdapter::new(
                fin_ethereum::EthereumConfig::wallet_only(1, 2, vec![]),
                vec![fin_ethereum::WalletConnection {
                    name: "fixture-wallet".into(),
                    address: "0x2222222222222222222222222222222222222222".into(),
                    signing_enabled: true,
                }],
                Arc::new(fin_evm::EvmRuntime::new(rpc, 1, None)),
            )
            .unwrap()
            .with_wallet_valuation(Arc::new(Prices)),
        );
        (
            KrakenEvmDepositAdapter::with_solana(
                kraken,
                BTreeMap::from([(1, ethereum)]),
                BTreeMap::new(),
            ),
            venue,
        )
    }
    fn input() -> Value {
        json!({"requestId":"synthetic-deposit-001","account":"fixture-spot","wallet":"fixture-wallet","methodId":"00000000-0000-0000-0000-000000000001","chainId":1,"amount":"0.01","maxFeeEth":"0.001"})
    }
    #[test]
    fn deposit_rpc_scope_keeps_solana_out_of_evm_deadline_policy() {
        let (adapter, _) = adapter();
        assert!(adapter.uses_rpc_reads("kraken.deposit", &input()));
        let solana = json!({
            "requestId":"synthetic-solana-deposit", "account":"fixture-spot",
            "wallet":"solana-wallet", "methodId":"synthetic-method",
            "network":"solana", "amount":"0.01", "maxFeeSol":"0.001",
        });
        assert!(!adapter.uses_rpc_reads("kraken.deposit", &solana));
        assert!(!adapter.uses_rpc_reads("kraken.deposit", &json!({})));
    }

    #[tokio::test]
    async fn current_route_alias_resolves_exact_wallet_and_canonical_venue_custody() {
        let (adapter, _) = adapter();
        let p = adapter.prepare("kraken.deposit", &input()).await.unwrap();
        let mut binding: DepositBinding = serde_json::from_value(p.binding.clone()).unwrap();
        binding.route["venue_presentation_rate"] = json!(0.125);
        let legacy_identity = json!({
            "method_id":binding.method_id,
            "method_name":binding.method_name,
            "network_id":binding.network_id,
            "network_name":binding.network_name,
            "chain_id":binding.chain_id,
            "address":binding.address,
            "venue_asset":binding.venue_asset,
            "gross":binding.gross,
            "net":binding.net,
            "fee":binding.fee,
            "fee_asset":binding.fee_asset,
        });
        assert_eq!(
            deposit_route_digest(&binding).unwrap(),
            binding_hash(&legacy_identity).unwrap()
        );
        assert!(p.binding.get("source_kind").is_none());
        assert!(p.binding.get("cluster").is_none());
        assert!(p.binding.get("mint").is_none());
        assert_eq!(p.binding["venue_asset"]["reference"], "XETH");
        assert_eq!(p.binding["source_asset"]["reference"], "native");
        assert_eq!(p.intent.debits[0].amount.atoms.to_decimal(18), "0.01");
        assert!(
            matches!(p.intent.expected.as_slice(),[EffectConstraint::Transfer{credit_asset,..}]if credit_asset.reference=="XETH")
        );
    }
    #[tokio::test]
    async fn shared_claimed_address_family_binds_the_selected_exact_address() {
        let (adapter, venue) = adapter();
        venue
            .multiple_claimed_addresses
            .store(true, Ordering::SeqCst);
        let mut selected = input();
        selected["address"] = json!("0x1111111111111111111111111111111111111111");
        let prepared = adapter
            .prepare("kraken.deposit", &selected)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error:?}; validated requests: {:?}",
                    venue.requests.lock().unwrap()
                )
            });
        assert_eq!(prepared.binding["address"], selected["address"]);
        let requests = venue.requests.lock().unwrap();
        let address_read = requests
            .iter()
            .find(|(endpoint, _)| *endpoint == E::KrakenFundingGetFundingV2DepositAddresses)
            .unwrap();
        assert_eq!(
            address_read.1["scope"]["method_id"],
            "00000000-0000-0000-0000-000000000003"
        );
    }
    #[tokio::test]
    async fn absent_account_maximum_remains_bounded_by_exact_preview() {
        let (adapter, venue) = adapter();
        venue
            .no_account_deposit_maximum
            .store(true, Ordering::SeqCst);
        let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
        assert!(prepared.warnings.iter().any(|warning| {
            warning.contains("did not return an account-specific deposit maximum")
                && warning.contains("exact approved principal")
        }));
        assert_eq!(
            prepared.intent.debits[0].amount.atoms.to_decimal(18),
            "0.01"
        );
    }
    #[tokio::test]
    async fn nonzero_deposit_fee_is_reserved_once_as_withheld_credit() {
        let (adapter, venue) = adapter();
        venue.fee.store(true, Ordering::SeqCst);
        let p = adapter.prepare("kraken.deposit", &input()).await.unwrap();
        assert_eq!(
            p.binding["net"],
            Quantity::from_decimal("0.0099", 36).unwrap().to_string()
        );
        assert!(
            p.intent
                .maximum_costs
                .iter()
                .any(|c| c.treatment == CostTreatment::DeductedFromCredit
                    && c.amount.atoms.to_decimal(36) == "0.0001")
        );
        let mut capped = input();
        capped["maxDepositFee"] = json!("0.00001");
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &capped)
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_FEE_LIMIT"
        );
    }
    #[tokio::test]
    async fn missing_readable_credit_permission_rejects_before_wallet_preparation() {
        let (adapter, venue) = adapter();
        venue.readable_credit.store(false, Ordering::SeqCst);
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &input())
                .await
                .unwrap_err()
                .code,
            "KRAKEN_PERMISSION_MISSING"
        );
        assert_eq!(
            venue.requests.lock().unwrap().as_slice(),
            &[(E::KrakenPrivatePostGetApiKeyInfo, json!({}))]
        );
        assert!(venue.rpc.transaction.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn memo_and_wrong_chain_are_rejected_before_wallet_preparation() {
        let (adapter, venue) = adapter();
        venue.memo.store(true, Ordering::SeqCst);
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &input())
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_MEMO_UNSUPPORTED"
        );
        venue.memo.store(false, Ordering::SeqCst);
        venue.wrong_network.store(true, Ordering::SeqCst);
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &input())
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_CHAIN_MISMATCH"
        );
    }
    #[tokio::test]
    async fn deposit_revalidation_reuses_frozen_wallet_envelope() {
        let (adapter, _) = adapter();
        let p = adapter.prepare("kraken.deposit", &input()).await.unwrap();
        let fresh = adapter.revalidate(&p).await.unwrap();
        assert_eq!(fresh.binding, p.binding);
    }
    #[test]
    fn schema_closes_source_and_route_controls() {
        let definition = deposit_definition();
        let schema = jsonschema::validator_for(&definition.input_schema).unwrap();
        assert!(schema.is_valid(&input()));
        let mut raw = input();
        raw["recipient"] = json!("0x3333333333333333333333333333333333333333");
        assert!(!schema.is_valid(&raw));
    }
    #[test]
    fn funding_status_accepts_only_exact_method_or_krakens_unified_suffix() {
        assert!(deposit_status_method_matches("USDC - Base", "USDC - Base"));
        assert!(deposit_status_method_matches(
            "USDC - Base",
            "USDC - Base (Unified)"
        ));
        assert!(!deposit_status_method_matches(
            "USDC - Base",
            "USDC - Ethereum (Unified)"
        ));
        assert!(!deposit_status_method_matches(
            "USDC - Base",
            "Other USDC - Base (Unified)"
        ));
    }
    #[test]
    fn hashed_funding_evidence_keeps_exact_amounts_and_omits_float_timestamps() {
        let entry = json!({
            "aclass":"currency","type":"deposit","subtype":"",
            "refid":"synthetic-ref","asset":"USDC","amount":"3.50000000",
            "fee":"0.00000000","balance":"3.51046496","time":1791808231.627728
        });
        let evidence = exact_ledger_evidence("synthetic-ledger", &entry);
        assert!(evidence.get("time").is_none());
        assert_eq!(evidence["amount"], "3.50000000");
        assert!(fin_domain::canonical_json(&evidence).is_ok());
    }
    async fn source_record(adapter: &KrakenEvmDepositAdapter, venue: &Venue) -> OperationRecord {
        source_record_for(adapter, venue, input()).await
    }
    async fn source_record_for(
        adapter: &KrakenEvmDepositAdapter,
        venue: &Venue,
        input: Value,
    ) -> OperationRecord {
        let p = adapter.prepare("kraken.deposit", &input).await.unwrap();
        let tx = p.binding["inner"]["binding"]["transaction"].clone();
        *venue.rpc.transaction.lock().unwrap() = Some(serde_json::from_value(tx.clone()).unwrap());
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("0x{}", "ab".repeat(32)),
            data: json!({"transaction":tx,"metadata":{"operation":"wallet.transfer","resource":p.binding["inner"]["binding"]["resource"]}}),
        };
        OperationRecord {
            id: "synthetic-deposit-record".into(),
            request_id: "synthetic-deposit-001".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: p.operation.clone(),
            input: p.input.clone(),
            status: Status::Pending,
            created_at: now(),
            updated_at: now(),
            preview: Some(p),
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
    #[tokio::test]
    async fn source_receipt_alone_does_not_complete_or_mint_venue_credit() {
        let (adapter, venue) = adapter();
        let record = source_record(&adapter, &venue).await;
        let waiting = adapter.reconcile(&record).await.unwrap();
        assert_eq!(waiting.status, Status::Pending);
        assert!(waiting.verified.is_none());
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        let source_only = adapter.reconcile(&record).await.unwrap();
        assert_eq!(source_only.status, Status::Pending);
        assert!(source_only.verified.is_none());
        assert_eq!(source_only.evidence["awaiting"], "venue_usable_credit");
        assert_eq!(source_only.evidence["resubmissionAllowed"], false);
        assert!(
            source_only.evidence["reason"]
                .as_str()
                .unwrap()
                .contains("confirmations")
        );
    }
    #[tokio::test]
    async fn settled_deposit_with_inconsistent_or_held_evidence_remains_unknown() {
        let (adapter, venue) = adapter();
        let record = source_record(&adapter, &venue).await;
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        for fields in [
            json!({"asset":"USDC"}),
            json!({"method":"Ether - Other Network"}),
            json!({"info":"0x2222222222222222222222222222222222222222"}),
            json!({"amount":"0.02"}),
            json!({"amount":"invalid"}),
            json!({"refid":""}),
            json!({"status-prop":"onhold"}),
            json!({"status-prop":"return"}),
            json!({"status-prop":42}),
            json!({"status":"Failure"}),
            json!({"status":"Unrecognized"}),
        ] {
            *venue.deposit_status_override.lock().unwrap() = fields.clone();
            let observed = adapter.reconcile(&record).await.unwrap();
            assert_eq!(observed.status, Status::Unknown, "{fields}");
            assert!(observed.verified.is_none());
            assert_eq!(observed.evidence["resubmissionAllowed"], false);
        }
    }
    #[tokio::test]
    async fn canonical_source_and_exact_usable_ledger_credit_settle_original_deposit() {
        let (adapter, venue) = adapter();
        let record = source_record(&adapter, &venue).await;
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        venue.credit.store(true, Ordering::SeqCst);
        let complete = adapter.reconcile(&record).await.unwrap();
        assert_eq!(complete.status, Status::Completed);
        let proof = complete.verified.unwrap();
        assert_eq!(proof.submission, record.submission.clone().unwrap());
        assert_eq!(proof.proof_digest, verified_effect_digest(&proof).unwrap());
        assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Transfer{to,credit,..}if to.0=="kraken:fixture-spot"&&credit.asset.reference=="XETH"&&credit.atoms.to_decimal(36)=="0.01")));
    }
    #[tokio::test]
    async fn kraken_early_confirmed_settles_only_with_exact_ledger_and_free_balance() {
        let (adapter, venue) = adapter();
        let record = source_record(&adapter, &venue).await;
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        venue.credit.store(true, Ordering::SeqCst);
        venue.early_confirmed.store(true, Ordering::SeqCst);

        let complete = adapter.reconcile(&record).await.unwrap();

        assert_eq!(complete.status, Status::Completed);
        let proof = complete.verified.unwrap();
        assert_eq!(proof.submission, record.submission.clone().unwrap());
        assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Transfer{to,credit,..}if to.0=="kraken:fixture-spot"&&credit.asset.reference=="XETH"&&credit.atoms.to_decimal(36)=="0.01")));
    }
    #[tokio::test]
    async fn deposit_fee_is_proven_by_correlated_ledger_and_withheld_exactly_once() {
        let (adapter, venue) = adapter();
        venue.fee.store(true, Ordering::SeqCst);
        let record = source_record(&adapter, &venue).await;
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        venue.credit.store(true, Ordering::SeqCst);
        let complete = adapter.reconcile(&record).await.unwrap();
        assert_eq!(complete.status, Status::Completed);
        let proof = complete.verified.unwrap();
        let fees=proof.effects.iter().filter(|e|matches!(e,FinancialEffect::Cost{category,treatment:CostTreatment::DeductedFromCredit,amount,..}if category=="kraken_deposit_fee"&&amount.atoms.to_decimal(36)=="0.0001")).count();
        assert_eq!(fees, 1);
        assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Transfer{credit,..}if credit.atoms.to_decimal(36)=="0.01")));
        assert_eq!(
            proof.evidence["freeAfter"],
            Quantity::from_decimal("0.5099", 36).unwrap().to_string()
        );
    }
    #[tokio::test]
    async fn tokenized_wallet_deposit_freezes_base_contract_unit_and_correlated_credit() {
        let (adapter, venue) = adapter();
        venue.tokenized.store(true, Ordering::SeqCst);
        venue.rpc.tokenized.store(true, Ordering::SeqCst);
        let mut input = input();
        input["token"] = json!("0x3333333333333333333333333333333333333333");
        input["denomination"] = json!("base");
        let record = source_record_for(&adapter, &venue, input.clone()).await;
        assert_eq!(
            record.preview.as_ref().unwrap().binding["venue_asset"]["network"],
            "kraken:tokenized_asset:base"
        );
        assert_eq!(
            record.preview.as_ref().unwrap().binding["source_asset"]["reference"],
            "0x3333333333333333333333333333333333333333"
        );
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        venue.credit.store(true, Ordering::SeqCst);
        let result = adapter.reconcile(&record).await.unwrap();
        assert_eq!(result.status, Status::Completed);
        assert!(result.verified.unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::Transfer{credit,..}if credit.asset.network=="kraken:tokenized_asset:base"&&credit.asset.reference=="SPYX.T")));
        {
            let calls = venue.requests.lock().unwrap();
            assert!(
                calls.iter().any(
                    |(e, p)| *e == E::KrakenPublicGetAssets && p["aclass"] == "tokenized_asset"
                )
            );
            assert!(
                calls
                    .iter()
                    .any(|(e, p)| *e == E::KrakenPrivatePostDepositStatus
                        && p["aclass"] == "tokenized_asset"
                        && p["rebase_multiplier"] == "base")
            );
        }
        input["denomination"] = json!("rebased");
        assert_eq!(
            adapter
                .prepare("kraken.deposit", &input)
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_DENOMINATION_UNSUPPORTED"
        );
    }
    #[tokio::test]
    async fn separate_deposit_fee_asset_requires_its_own_proof_and_books_one_additional_cost() {
        let (adapter, venue) = adapter();
        venue.separate_fee.store(true, Ordering::SeqCst);
        let record = source_record(&adapter, &venue).await;
        assert!(
            record
                .preview
                .as_ref()
                .unwrap()
                .intent
                .maximum_costs
                .iter()
                .any(|cost| cost.amount.asset.reference == "USDC"
                    && cost.treatment == CostTreatment::AdditionalDebit)
        );
        assert_eq!(
            record.preview.as_ref().unwrap().binding["net"],
            record.preview.as_ref().unwrap().binding["gross"]
        );
        venue.rpc.receipt.store(true, Ordering::SeqCst);
        venue.credit.store(true, Ordering::SeqCst);
        venue.fee_proven.store(false, Ordering::SeqCst);
        let pending = adapter.reconcile(&record).await.unwrap();
        assert_eq!(pending.status, Status::Unknown);
        assert!(pending.verified.is_none());
        venue.fee_proven.store(true, Ordering::SeqCst);
        let complete = adapter.reconcile(&record).await.unwrap();
        assert_eq!(complete.status, Status::Completed);
        let proof = complete.verified.unwrap();
        assert_eq!(proof.effects.iter().filter(|effect|matches!(effect,FinancialEffect::Cost{amount,treatment:CostTreatment::AdditionalDebit,category,..}if category=="kraken_deposit_fee"&&amount.asset.reference=="USDC")).count(),1);
        assert!(!proof.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Cost {
                treatment: CostTreatment::DeductedFromCredit,
                ..
            }
        )));
        assert!(proof.effects.iter().any(|effect|matches!(effect,FinancialEffect::Transfer{credit,..}if credit.atoms.to_decimal(36)=="0.01")));
    }
}
