//! A frozen Ethereum-family transfer completes only after exact Coinbase usable credit.
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_coinbase::{
    CoinbaseProduct, CoinbaseProfile, CoinbaseQuery, CoinbaseRequest, CoinbaseRoute,
};
use gofin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, Definition, EffectConstraint, ExecutionActor,
    FinancialEffect, FinancialIntent, FinancialState, GoFinError, OperationKind, OperationRecord,
    PreparedOperation, Quantity, Reconciliation, ResourceId, Result, RevalidatedOperation,
    StateSnapshot, Status, Submission, SubmissionJournal, now, verified_effect_digest,
};
use gofin_exchange::ExchangeProfile;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

const BASE_USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CoinbaseDepositNetwork {
    Ethereum,
    Arbitrum,
    Optimism,
    Polygon,
    #[serde(alias = "avalanche-c-chain")]
    Avalanche,
    Arc,
    HyperEvm,
    #[default]
    Base,
}
impl CoinbaseDepositNetwork {
    fn chain_id(self) -> u64 {
        match self {
            Self::Ethereum => 1,
            Self::Base => 8453,
            Self::Arbitrum => 42161,
            Self::Optimism => 10,
            Self::Polygon => 137,
            Self::Avalanche => 43114,
            Self::Arc => 5042,
            Self::HyperEvm => 999,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Base => "base",
            Self::Arbitrum => "arbitrum",
            Self::Optimism => "optimism",
            Self::Polygon => "polygon",
            Self::Avalanche => "avalanche",
            Self::Arc => "arc",
            Self::HyperEvm => "hyperevm",
        }
    }
    fn gas_asset(self) -> &'static str {
        match self {
            Self::Ethereum | Self::Base | Self::Arbitrum | Self::Optimism => "ETH",
            Self::Polygon => "POL",
            Self::Avalanche => "AVAX",
            Self::Arc => "USDC",
            Self::HyperEvm => "HYPE",
        }
    }
    fn token(self) -> &'static str {
        match self {
            Self::Ethereum => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            Self::Base => BASE_USDC,
            Self::Arbitrum => "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
            Self::Optimism => "0x0b2c639c533813f4aa9d7837caf62653d097ff85",
            Self::Polygon => "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359",
            Self::Avalanche => "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e",
            Self::Arc => "native",
            Self::HyperEvm => "0xb88339cb7199b77e23db6e890353e22632ba630f",
        }
    }
    fn eurc_token(self) -> Option<&'static str> {
        match self {
            Self::Ethereum => Some("0x1abaea1f7c830bd89acc67ec4af516284b1bc33c"),
            Self::Base => Some("0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42"),
            Self::Avalanche => Some("0xc891eb4cbdeff6e073e859e987815ed1505c2acd"),
            Self::Arc => Some("0xbef5f6d51cb62b58e6a8f77868681825c6fe21c1"),
            _ => None,
        }
    }
    fn from_chain_id(chain_id: u64) -> Result<Self> {
        match chain_id {
            1 => Ok(Self::Ethereum),
            8453 => Ok(Self::Base),
            42161 => Ok(Self::Arbitrum),
            10 => Ok(Self::Optimism),
            137 => Ok(Self::Polygon),
            43114 => Ok(Self::Avalanche),
            5042 => Ok(Self::Arc),
            999 => Ok(Self::HyperEvm),
            _ => Err(error(
                "COINBASE_DEPOSIT_NETWORK_UNAVAILABLE",
                "The frozen source network is unsupported.",
            )),
        }
    }
}
fn deposit_token(input: &CoinbaseDepositInput, network: CoinbaseDepositNetwork) -> Result<String> {
    if input.currency.is_empty()
        || input.currency.len() > 16
        || !input
            .currency
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Err(GoFinError::validation(
            "Currency must be an exact uppercase venue symbol.",
        ));
    }
    if matches!(input.currency.as_str(), "USDC" | "EURC") {
        let expected = if input.currency == "EURC" {
            network.eurc_token().ok_or_else(|| {
                error(
                    "COINBASE_DEPOSIT_ASSET_UNAVAILABLE",
                    "EURC is not issued on the selected network.",
                )
            })?
        } else {
            network.token()
        };
        if let Some(explicit) = &input.token
            && !(expected.eq_ignore_ascii_case(explicit)
                || (input.currency == "USDC"
                    && network == CoinbaseDepositNetwork::Arc
                    && explicit.eq_ignore_ascii_case("0x3600000000000000000000000000000000000000")))
        {
            return Err(GoFinError::validation(
                "Stablecoin deposits must use their canonical network token.",
            ));
        }
        return Ok(
            if input.currency == "USDC" && network == CoinbaseDepositNetwork::Arc {
                "native".into()
            } else {
                expected.into()
            },
        );
    }
    let token = input.token.as_deref().ok_or_else(|| {
        GoFinError::validation(
            "Non-USDC/EURC deposits require an explicit ERC-20 contract address.",
        )
    })?;
    Ok(gofin_evm::lower_address(gofin_evm::address(token)?))
}
fn network_label_matches(label: &str, network: CoinbaseDepositNetwork) -> bool {
    label == network.name()
        || (network == CoinbaseDepositNetwork::Avalanche && label == "avalanche-c-chain")
        || (network == CoinbaseDepositNetwork::Arbitrum && label == "arbitrum-one")
}
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoinbaseDepositInput {
    #[serde(default)]
    pub network: CoinbaseDepositNetwork,
    pub account: String,
    pub wallet: String,
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub account_id: String,
    pub address_id: String,
    pub amount: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fee_eth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fee_native: Option<String>,
}
fn default_currency() -> String {
    "USDC".into()
}
impl CoinbaseDepositInput {
    fn max_fee(&self) -> Result<&str> {
        match (self.max_fee_native.as_deref(), self.max_fee_eth.as_deref()) {
            (Some(fee), None) | (None, Some(fee)) if !fee.is_empty() => Ok(fee),
            _ => Err(GoFinError::validation(
                "Provide exactly one of maxFeeNative or legacy maxFeeEth.",
            )),
        }
    }
}
pub fn definition() -> Definition {
    Definition { name: "coinbase.deposit".into(),
        description: "Deposit a supported token from an enrolled Ethereum, Base, Arbitrum, Optimism, Polygon, Avalanche, Arc or HyperEVM wallet into an existing Coinbase address and verify usable credit.".into(),
        kind: OperationKind::Write, execution_actor: ExecutionActor::AgentInvocable,
        input_schema: schema_for!(CoinbaseDepositInput).into(), output_schema: json!({"type":"object"}),
        error_schema: schema_for!(GoFinError).into(),
        examples: vec![json!({"account":"coinbase","wallet":"wallet","network":"polygon","accountId":"currency-account-uuid",
            "addressId":"existing-evm-address-id","amount":"5","requestId":"deposit-001","maxFeeNative":"0.01"})],
        coverage: "USDC and EURC may use shared Coinbase EVM deposit addresses whose metadata names another EVM network; other explicitly selected ERC-20 assets require an address on the selected source network.".into(),
        completion: "Canonical source transfer, transaction-hash-correlated completed receive and independently observed usable Coinbase credit.".into(),
        semantics: json!({"supportedNetworks":{"ethereum":{"chainId":1,"sourceContract":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","decimals":6,"gasAsset":"ETH"},"base":{"chainId":8453,"sourceContract":BASE_USDC,"decimals":6,"gasAsset":"ETH"},"arbitrum":{"chainId":42161,"sourceContract":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6,"gasAsset":"ETH"},"optimism":{"chainId":10,"sourceContract":"0x0b2c639c533813f4aa9d7837caf62653d097ff85","decimals":6,"gasAsset":"ETH"},"polygon":{"chainId":137,"sourceContract":"0x3c499c542cef5e3811e1192ce70d8cc03d5c3359","decimals":6,"gasAsset":"POL"},"avalanche":{"chainId":43114,"sourceContract":"0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e","decimals":6,"gasAsset":"AVAX"},"arc":{"chainId":5042,"sourceAsset":"native","decimals":18,"gasAsset":"USDC"},"hyperevm":{"chainId":999,"sourceContract":"0xb88339cb7199b77e23db6e890353e22632ba630f","decimals":6,"gasAsset":"HYPE"}},"supportedStablecoins":{"eurc":{"ethereum":"0x1abaea1f7c830bd89acc67ec4af516284b1bc33c","base":"0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42","avalanche":"0xc891eb4cbdeff6e073e859e987815ed1505c2acd","arc":"0xbef5f6d51cb62b58e6a8f77868681825c6fe21c1"}},"defaultNetwork":"base",
            "addressCreation":"separate typed operation","sourceReceiptAloneCompletes":false,
            "depositAddressNetwork":{"usdcAndEurc":"Coinbase address metadata may name another EVM network; receive evidence must still match the selected source network.","otherCurrencies":"Address metadata must exactly match the selected source network."},"currency":{"default":"USDC","nonStablecoinCurrenciesRequireExplicitToken":true,"tokenDecimals":"verified from wallet runtime metadata"},"fee":{"maxFeeNative":"amount in selected network gas asset","legacyMaxFeeEth":"supported only where gas asset is ETH"},
            "unknownOutcome":"reconcile the original transaction; never replay"}) }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    account: String,
    account_id: String,
    address_id: String,
    address: String,
    resource: ResourceId,
    physical_identity: String,
    venue_asset: AssetId,
    gross: Quantity,
    free_before: Quantity,
    inner: PreparedOperation,
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    value_bounds: Option<Value>,
}
pub struct CoinbaseEvmDepositAdapter {
    profile: Arc<CoinbaseProfile>,
    networks: Vec<Arc<gofin_ethereum::EthereumAdapter>>,
}
pub type CoinbaseBaseDepositAdapter = CoinbaseEvmDepositAdapter;
fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "coinbase_deposit",
        message,
        "Inspect the original source and Coinbase transaction identities; never repeat an uncertain deposit.",
    )
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                "Exact Coinbase deposit evidence is missing.",
            )
        })
}
fn pending(reason: &str, detail: Value) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        verified: None,
        evidence: json!({"reason":reason,"detail":detail,"awaiting":"venue_usable_credit","resubmissionAllowed":false}),
    }
}
// An address-family label is not the transfer's chain identity. Coinbase can
// return its shared EVM receive address under another EVM network's name.
fn evm_address_network(network: &str) -> bool {
    matches!(
        network,
        "ethereum"
            | "base"
            | "arbitrum"
            | "arbitrum-one"
            | "optimism"
            | "polygon"
            | "avalanche"
            | "avalanche-c-chain"
            | "bsc"
            | "binance-smart-chain"
            | "celo"
            | "zksync"
            | "linea"
            | "scroll"
            | "mantle"
            | "worldchain"
            | "ronin"
            | "unichain"
            | "blast"
            | "ink"
            | "sonic"
            | "gnosis"
            | "fantom"
            | "arc"
            | "hyperevm"
    ) || network.strip_prefix("eip155:").is_some_and(|chain| {
        !chain.is_empty()
            && chain.bytes().all(|byte| byte.is_ascii_digit())
            && chain.parse::<u64>().is_ok_and(|chain| chain > 0)
    })
}
impl CoinbaseEvmDepositAdapter {
    pub fn new(
        profile: Arc<CoinbaseProfile>,
        base: Arc<gofin_ethereum::EthereumAdapter>,
    ) -> Result<Self> {
        if base.config.chain_id != 8453 {
            return Err(GoFinError::validation(
                "Coinbase Base funding requires the Base runtime.",
            ));
        }
        Self::with_networks(profile, vec![base])
    }
    pub fn with_networks(
        profile: Arc<CoinbaseProfile>,
        networks: Vec<Arc<gofin_ethereum::EthereumAdapter>>,
    ) -> Result<Self> {
        if networks.is_empty()
            || networks.iter().any(|adapter| {
                !matches!(
                    adapter.config.chain_id,
                    1 | 10 | 8453 | 42161 | 137 | 43114 | 5042 | 999
                )
            })
        {
            return Err(GoFinError::validation(
                "Coinbase deposit requires supported Ethereum-family runtimes.",
            ));
        }
        let mut ids = BTreeSet::new();
        if networks
            .iter()
            .any(|adapter| !ids.insert(adapter.config.chain_id))
        {
            return Err(GoFinError::validation(
                "Coinbase deposit runtimes must have unique chain IDs.",
            ));
        }
        Ok(Self { profile, networks })
    }
    fn runtime(
        &self,
        network: CoinbaseDepositNetwork,
    ) -> Result<&Arc<gofin_ethereum::EthereumAdapter>> {
        self.networks
            .iter()
            .find(|adapter| adapter.config.chain_id == network.chain_id())
            .ok_or_else(|| {
                error(
                    "COINBASE_DEPOSIT_NETWORK_UNAVAILABLE",
                    "The selected source network is not configured.",
                )
            })
    }
    fn frozen_network(&self, inner: &PreparedOperation) -> Result<CoinbaseDepositNetwork> {
        let chain = inner
            .input
            .get("chain_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                error(
                    "COINBASE_DEPOSIT_BINDING_INVALID",
                    "Frozen source network is missing.",
                )
            })?;
        CoinbaseDepositNetwork::from_chain_id(chain)
    }
    async fn read(&self, account: &str, route: CoinbaseRoute) -> Result<Value> {
        let response = self
            .profile
            .request_for_account(
                account,
                CoinbaseProduct::Advanced,
                route,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(error(
                "COINBASE_DEPOSIT_READ_FAILED",
                "Coinbase funding evidence is unavailable or access was denied.",
            ));
        }
        serde_json::from_slice(&response.body).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                "Funding evidence is malformed.",
            )
        })
    }
    async fn balance(&self, account: &str, account_id: &str, currency: &str) -> Result<Quantity> {
        let value = self
            .read(
                account,
                CoinbaseRoute::Account {
                    account_id: account_id.into(),
                },
            )
            .await?;
        let row = &value["account"];
        if row["uuid"] != account_id
            || row["currency"].as_str() != Some(currency)
            || row["active"].as_bool() != Some(true)
            || row["ready"].as_bool() != Some(true)
            || row["available_balance"]["currency"].as_str() != Some(currency)
        {
            return Err(error(
                "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                "Currency account identity or usable denomination differs from the selected route.",
            ));
        }
        Quantity::from_decimal(text(&row["available_balance"], "value")?, 36)
    }
    async fn app_rows(&self, account: &str, currency_id: Option<&str>) -> Result<Vec<Value>> {
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        for _ in 0..64 {
            let query = CoinbaseQuery {
                limit: Some(100),
                starting_after: cursor.clone(),
                ..Default::default()
            };
            let route = match currency_id {
                Some(id) => CoinbaseRoute::Transactions {
                    account_id: id.into(),
                    query,
                },
                None => CoinbaseRoute::LegacyAccounts { query },
            };
            let value = self.read(account, route).await?;
            rows.extend(
                value["data"]
                    .as_array()
                    .ok_or_else(|| {
                        error(
                            "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                            "App funding list is missing.",
                        )
                    })?
                    .iter()
                    .cloned(),
            );
            match value["pagination"]["next_starting_after"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                None => return Ok(rows),
                Some(next) if seen.insert(next.to_owned()) => cursor = Some(next.to_owned()),
                _ => {
                    return Err(error(
                        "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
                        "App funding pagination repeats a cursor.",
                    ));
                }
            }
        }
        Err(error(
            "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
            "App funding history exceeds the bounded page scope.",
        ))
    }
    async fn route(&self, input: &CoinbaseDepositInput) -> Result<(String, String, ResourceId)> {
        let account = self
            .profile
            .accounts()
            .iter()
            .find(|row| {
                row.name == input.account
                    && row.product == CoinbaseProduct::Advanced
                    && row.identity_verified
            })
            .ok_or_else(|| {
                error(
                    "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                    "An authenticated Coinbase portfolio is required.",
                )
            })?;
        let permissions = self
            .read(&input.account, CoinbaseRoute::KeyPermissions)
            .await?;
        if permissions["portfolio_uuid"] != account.physical_identity
            || permissions["can_view"] != true
        {
            return Err(error(
                "COINBASE_DEPOSIT_PORTFOLIO_MISMATCH",
                "Current permissions do not attest the original readable portfolio.",
            ));
        }
        let rows = self.app_rows(&input.account, None).await?;
        let selected = rows
            .iter()
            .filter(|row| {
                row["id"] == input.account_id
                    && row["currency"]["code"].as_str() == Some(&input.currency)
            })
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(error(
                "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                "App UUID must independently attest the exact selected currency account.",
            ));
        }
        self.balance(&input.account, &input.account_id, &input.currency)
            .await?;
        let value = self
            .read(
                &input.account,
                CoinbaseRoute::DepositAddresses {
                    account_id: input.account_id.clone(),
                },
            )
            .await?;
        if value["pagination"]["next_starting_after"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            return Err(error(
                "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
                "Deposit addresses exceed the complete observed page.",
            ));
        }
        let rows = value["data"].as_array().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_ADDRESS_INVALID",
                "Address list is missing.",
            )
        })?;
        let selected = rows
            .iter()
            .filter(|row| row["id"] == input.address_id)
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(error(
                "COINBASE_DEPOSIT_ADDRESS_INVALID",
                "Select one existing Coinbase EVM deposit address.",
            ));
        }
        let row = selected[0];
        let address_currency = row["currency"]
            .as_str()
            .or_else(|| row["currency"]["code"].as_str());
        if address_currency.is_some_and(|currency| currency != input.currency) {
            return Err(error(
                "COINBASE_DEPOSIT_ADDRESS_INVALID",
                "Address currency metadata differs from the selected Coinbase account currency.",
            ));
        }
        let address_network = row["network"]
            .as_str()
            .or_else(|| row["network"]["name"].as_str());
        let network_matches = if matches!(input.currency.as_str(), "USDC" | "EURC") {
            address_network.is_some_and(evm_address_network)
        } else {
            address_network.is_some_and(|name| network_label_matches(name, input.network))
        };
        if !network_matches
            || row
                .get("destination_tag")
                .is_some_and(|tag| !tag.is_null() && tag.as_str() != Some(""))
        {
            return Err(error(
                "COINBASE_DEPOSIT_NETWORK_UNVERIFIED",
                "USDC/EURC accept a recognized shared EVM address; other currencies require address metadata matching the source network, and no destination tag.",
            ));
        }
        Ok((
            gofin_evm::lower_address(gofin_evm::address(text(row, "address")?)?),
            account.physical_identity.clone(),
            account.resource.clone(),
        ))
    }
    async fn prepare_deposit(
        &self,
        value: &Value,
        old: Option<&Binding>,
    ) -> Result<PreparedOperation> {
        let input: CoinbaseDepositInput = serde_json::from_value(value.clone())
            .map_err(|_| GoFinError::validation("Use the closed coinbase.deposit schema."))?;
        if input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(GoFinError::validation(
                "A bounded deposit requestId is required.",
            ));
        }
        let network = input.network;
        if input.max_fee_eth.is_some() && network.gas_asset() != "ETH" {
            return Err(GoFinError::validation(
                "Legacy maxFeeEth is only valid where the gas asset is ETH; use maxFeeNative elsewhere.",
            ));
        }
        let gross = Quantity::from_decimal(&input.amount, 36)?;
        if gross.is_zero() {
            return Err(GoFinError::validation("Deposit amount must be positive."));
        }
        let (address, physical_identity, resource) = self.route(&input).await?;
        let free_before = self
            .balance(&input.account, &input.account_id, &input.currency)
            .await?;
        let runtime = self.runtime(network)?;
        let token = deposit_token(&input, network)?;
        let max_fee = input.max_fee()?.to_owned();
        let inner_input = json!({"wallet":input.wallet,"network":"evm","chain_id":network.chain_id(),"request_id":input.request_id,
            "max_fee_eth":max_fee,"token":token,"amount":input.amount,"recipient":address});
        let inner = if let Some(binding) = old {
            if binding.inner.input != inner_input {
                return Err(error(
                    "PREVIEW_CHANGED",
                    "Deposit destination differs from the approved envelope.",
                ));
            }
            let frozen_network = self.frozen_network(&binding.inner)?;
            if frozen_network != network {
                return Err(error(
                    "PREVIEW_CHANGED",
                    "Deposit source network differs from the approved envelope.",
                ));
            }
            self.runtime(frozen_network)?
                .revalidate(&binding.inner)
                .await?
        } else {
            runtime.prepare("wallet.transfer", &inner_input).await?
        };
        let source = inner.intent.debits.first().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_SOURCE_INVALID",
                "Wallet preview omitted its source debit.",
            )
        })?;
        if source.amount.asset.decimals > 36 {
            return Err(error(
                "COINBASE_DEPOSIT_SOURCE_INVALID",
                "Token precision above the supported 36 decimal accounting scale is not supported.",
            ));
        }
        let source_atoms = Quantity::from_decimal(&input.amount, source.amount.asset.decimals)?;
        if source.amount.atoms != source_atoms
            || source.amount.asset.network != format!("ethereum:{}", network.chain_id())
            || !source.amount.asset.reference.eq_ignore_ascii_case(&token)
            || (matches!(input.currency.as_str(), "USDC" | "EURC")
                && source.amount.asset.decimals
                    != if input.currency == "USDC" && network == CoinbaseDepositNetwork::Arc {
                        18
                    } else {
                        6
                    })
        {
            return Err(error(
                "COINBASE_DEPOSIT_SOURCE_INVALID",
                "Wallet preview differs from the selected currency, canonical token and amount.",
            ));
        }
        let network_warning = if matches!(input.currency.as_str(), "USDC" | "EURC") {
            format!(
                "Coinbase may label its shared {} EVM address with another network. The source and required receive remain bound to {}.",
                input.currency,
                network.name()
            )
        } else {
            format!(
                "This {} address must match {} metadata exactly; source and receive evidence remain bound to that network.",
                input.currency,
                network.name()
            )
        };
        let venue_asset = AssetId {
            network: "coinbase".into(),
            reference: input.currency.clone(),
            decimals: 36,
        };
        let mut locks = inner.intent.locks.clone();
        locks.push(resource.clone());
        locks.sort();
        locks.dedup();
        let intent = FinancialIntent {
            debits: inner.intent.debits.clone(),
            maximum_costs: inner.intent.maximum_costs.clone(),
            locks,
            expected: vec![EffectConstraint::Transfer {
                from: source.resource.clone(),
                to: resource.clone(),
                debit_asset: source.amount.asset.clone(),
                credit_asset: venue_asset.clone(),
                maximum_debit: source_atoms,
                minimum_credit: gross.clone(),
            }],
        };
        let binding = Binding {
            account: input.account.clone(),
            account_id: input.account_id,
            address_id: input.address_id,
            address,
            resource,
            physical_identity,
            venue_asset,
            gross,
            free_before,
            inner: inner.clone(),
            value_bounds: None,
        };
        let mut prepared = PreparedOperation { operation: "coinbase.deposit".into(), input: value.clone(),
            binding: serde_json::to_value(binding).map_err(|_| GoFinError::validation("Deposit serialization failed."))?,
            summary: format!("Deposit {} {} ({}) from {} on {}; gas cap {} {}; credit Coinbase {}", input.amount, input.currency, token, input.wallet, network.name(), max_fee, network.gas_asset(), input.account),
            checks: vec![Check { name: "exact_evm_route".into(), status: CheckStatus::Passed,
                detail: format!("Existing Coinbase-owned address, exact App/Advanced UUID, permissioned portfolio and frozen {} {} source envelope verified.", network.name(), input.currency) }],
            warnings: vec![network_warning,
                "Source inclusion does not establish usable Coinbase credit. Any unexpected venue deduction retains the original pending identity.".into()],
            observed_at: inner.observed_at, expires_at: inner.expires_at, valuation: inner.valuation,
            intent, operation_version: 1, action_deadline: None };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}
#[async_trait]
impl Adapter for CoinbaseEvmDepositAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![definition()]
    }
    fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        let mut rows = self.profile.authorization_identities();
        for network in &self.networks {
            rows.extend(network.authorization_identities());
        }
        rows
    }
    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        // The constituent adapters observe custody. Reusing their identities
        // here would select this composition as a second scoped observer.
        vec![]
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec!["Composition owns no separate holdings; the enrolled wallet and Coinbase profile observe their exact custody resources.".into()],
            pending: vec![],
        })
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(GoFinError::validation(
            "Coinbase deposit is a financial write; preview it first.",
        ))
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != "coinbase.deposit" {
            return Err(GoFinError::validation("Unsupported deposit operation."));
        }
        self.prepare_deposit(input, None).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        if prepared.operation != "coinbase.deposit"
            || prepared.operation_version != 1
            || DateTime::parse_from_rfc3339(&prepared.expires_at)
                .map_or(true, |expiry| expiry <= Utc::now())
        {
            return Err(error(
                "PREVIEW_EXPIRED",
                "Original deposit preview expired or has an unsupported version.",
            ));
        }
        let old: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        let mut fresh = self.prepare_deposit(&prepared.input, Some(&old)).await?;
        let new: Binding = serde_json::from_value(fresh.binding.clone())
            .map_err(|_| GoFinError::validation("Fresh binding is malformed."))?;
        if old.address != new.address
            || old.resource != new.resource
            || old.physical_identity != new.physical_identity
            || old.free_before != new.free_before
            || old.inner.binding != new.inner.binding
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "Deposit route, custody, balance or source envelope changed.",
            ));
        }
        gofin_domain::value_bounds::carry(prepared, &mut fresh)?;
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let validated = RevalidatedOperation::after_revalidation(
            self.revalidate(prepared).await?,
            Arc::new(gofin_domain::clock::SystemClock),
        )?;
        self.submit_revalidated(&validated, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        journal.assert_candidate(prepared)?;
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        if prepared.operation != "coinbase.deposit"
            || prepared.operation_version != 1
            || binding.inner.operation != "wallet.transfer"
        {
            return Err(error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Deposit must bind one exact wallet transfer.",
            ));
        }
        let transaction: gofin_evm::FrozenTransaction =
            serde_json::from_value(binding.inner.binding["transaction"].clone()).map_err(|_| {
                error(
                    "COINBASE_DEPOSIT_BINDING_INVALID",
                    "Source envelope is malformed.",
                )
            })?;
        let network = self.frozen_network(&binding.inner)?;
        self.runtime(network)?.runtime.submit_nested_revalidated(&transaction,
            json!({"operation":"wallet.transfer","resource":text(&binding.inner.binding,"resource")?}), journal, validated, "/inner/binding").await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let prepared = record.preview.as_ref().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original preview is missing.",
            )
        })?;
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_SUBMISSION_MISSING",
                "Original source transaction is missing.",
            )
        })?;
        if !self.profile.accounts().iter().any(|row| {
            row.name == binding.account
                && row.resource == binding.resource
                && row.physical_identity == binding.physical_identity
                && row.product == CoinbaseProduct::Advanced
        }) {
            return Err(error(
                "COINBASE_DEPOSIT_IDENTITY_CHANGED",
                "Original Coinbase portfolio is no longer configured.",
            ));
        }
        let network = self.frozen_network(&binding.inner)?;
        let source = self
            .runtime(network)?
            .reconcile(&OperationRecord {
                operation: "wallet.transfer".into(),
                input: binding.inner.input.clone(),
                preview: Some(binding.inner.clone()),
                ..record.clone()
            })
            .await?;
        if source.status != Status::Completed {
            return Ok(source);
        }
        let Some(mut proof) = source.verified else {
            return Ok(pending(
                "Canonical source lacks typed effects.",
                source.evidence,
            ));
        };
        if proof.submission != *submission {
            return Err(error(
                "COINBASE_DEPOSIT_PROOF_MISMATCH",
                "Source evidence differs from the original submission.",
            ));
        }
        let rows = self
            .app_rows(&binding.account, Some(&binding.account_id))
            .await?;
        let selected = rows
            .iter()
            .filter(|row| {
                row["network"]["hash"]
                    .as_str()
                    .or_else(|| row["network"]["transaction_hash"].as_str())
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(&submission.reference))
            })
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Ok(pending(
                "Exactly one receive must correlate to the original source hash.",
                source.evidence,
            ));
        }
        let row = selected[0];
        let receive_network = row["network"]["name"]
            .as_str()
            .or_else(|| row["network"]["network"].as_str())
            .or_else(|| row["network"]["network_name"].as_str());
        if row["type"] != "receive"
            || row["status"] != "completed"
            || !receive_network.is_some_and(|name| network_label_matches(name, network))
            || row["amount"]["currency"].as_str() != Some(&binding.venue_asset.reference)
            || row["to"]["address"]
                .as_str()
                .is_none_or(|address| !address.eq_ignore_ascii_case(&binding.address))
        {
            return Ok(pending(
                "Receive is not completed with the original source network identity.",
                json!({"source":source.evidence,"venue":row}),
            ));
        }
        let credit = Quantity::from_decimal(text(&row["amount"], "amount")?, 36)?;
        if credit != binding.gross
            || self
                .balance(
                    &binding.account,
                    &binding.account_id,
                    &binding.venue_asset.reference,
                )
                .await?
                < binding.free_before.checked_add(&credit)?
        {
            return Ok(pending(
                "Exact selected-currency credit is not independently available for trading.",
                json!({"source":source.evidence,"venue":row}),
            ));
        }
        let debit = binding.inner.intent.debits.first().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original source debit is missing.",
            )
        })?;
        let recipient = ResourceId(format!(
            "ethereum:{}:{}",
            network.chain_id(),
            binding.address
        ));
        let mut effects = vec![];
        let mut matched = false;
        for effect in proof.effects {
            match effect {
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit: actual_debit,
                    credit: actual_credit,
                } if !matched
                    && from == debit.resource
                    && to == recipient
                    && actual_debit == debit.amount
                    && actual_credit == actual_debit =>
                {
                    matched = true;
                    effects.push(FinancialEffect::Transfer {
                        from,
                        to: binding.resource.clone(),
                        debit: actual_debit,
                        credit: Amount {
                            asset: binding.venue_asset.clone(),
                            atoms: credit.clone(),
                        },
                    });
                }
                FinancialEffect::Cost { .. } => effects.push(effect),
                _ => {
                    return Err(error(
                        "COINBASE_DEPOSIT_PROOF_MISMATCH",
                        "Unexpected source effects cannot become Coinbase custody credit.",
                    ));
                }
            }
        }
        if !matched {
            return Err(error(
                "COINBASE_DEPOSIT_PROOF_MISMATCH",
                "Canonical source omitted the exact deposit transfer.",
            ));
        }
        proof.effects = effects;
        proof.evidence = json!({"method":"canonical-evm-and-coinbase-usable-credit-v1","source":source.evidence,
            "venue":{"id":row["id"],"type":row["type"],"status":row["status"],"amount":row["amount"],
                "network":{"name":network.name(),"hash":submission.reference},"to":{"address":binding.address}},"freeBefore":binding.free_before});
        proof.observed_at = now();
        proof.proof_digest = verified_effect_digest(&proof)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: proof.evidence.clone(),
            verified: Some(proof),
        })
    }
}
